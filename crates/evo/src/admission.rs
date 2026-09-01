// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The admission engine.
//!
//! The admission engine owns the admitted plugins and runs their
//! lifecycles. It supports in-process singleton respondents and
//! in-process singleton wardens, plus out-of-process singleton
//! respondents and wardens over the wire protocol. Factories and
//! multi-plugin shelves are future work.
//!
//! ## Type erasure
//!
//! The SDK's public traits (`Plugin`, `Respondent`, `Warden`) use
//! native async in traits with `impl Future + Send` return types.
//! Those traits are not object-safe (native async traits cannot be
//! `dyn`) so the admission engine cannot hold `Box<dyn Respondent>`
//! or `Box<dyn Warden>` directly.
//!
//! The engine solves this with a pair of internal object-safe traits,
//! [`ErasedRespondent`] and [`ErasedWarden`], and generic adapters
//! [`RespondentAdapter`] and [`WardenAdapter`] that implement them
//! for any concrete plugin type. An [`AdmittedHandle`] enum carries
//! exactly one of the two variants per admission, decided from the
//! manifest's `kind.interaction`. This keeps the public SDK traits
//! zero-allocation while letting the engine store heterogeneous
//! plugins in a single collection.

mod erasure;
mod handle;
pub mod reprobe;
mod spawn;
mod validation;

pub use erasure::{
    ErasedRespondent, ErasedWarden, ErasedWardenAndRespondent,
    RespondentAdapter, WardenAdapter, WardenAndRespondentAdapter,
};
pub use handle::AdmittedHandle;

use spawn::{kill_holdout_child, unload_one_plugin, wait_for_socket_ready};

use crate::admin::AdminLedger;
use crate::catalogue::Catalogue;
use crate::config::PluginsSecurityConfig;
use crate::context::{
    LoggingInstanceAnnouncer, LoggingStateReporter,
    LoggingUserInteractionRequester, RegistryRelationAdmin,
    RegistryRelationAnnouncer, RegistrySubjectAdmin, RegistrySubjectAnnouncer,
    RegistrySubjectQuerier,
};
use crate::custody::CustodyLedger;
use crate::error::StewardError;
use crate::factory::RegistryInstanceAnnouncer;
use crate::happenings::HappeningBus;
use crate::persistence::PersistenceStore;
use crate::plugin_trust::PluginTrustState;
use crate::projections::SubjectConflictIndex;
use crate::queue::{QueueError, UriSchemeRegistry};
use crate::relations::RelationGraph;
use crate::router::{EnforcementPolicy, PluginEntry, PluginRouter};
use crate::state::StewardState;
use crate::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::factory::Factory;
use evo_plugin_sdk::contract::{
    HealthReport, InstanceAnnouncer, LoadContext, Respondent, SubjectAnnouncer,
    Warden,
};
use evo_plugin_sdk::manifest::{
    ArtefactKind, InstanceShape, InteractionShape, StockingRole, TransportKind,
};
use evo_plugin_sdk::Manifest;

/// Per-shelf admission-time conflict check.
///
/// Wraps [`PluginRouter::would_conflict_with_admission`] so the
/// admission paths can share one verb-aware check across every
/// shape (singleton respondent / warden / factory / OOP / …).
///
/// Derives the `(role, verbs)` tuple from the manifest's stocking
/// matching `shelf`, falling back to the manifest's
/// `kind.interaction` + `capabilities.respondent.request_types`
/// for legacy `[target]`-only manifests. Returns the conflict
/// message when admission would refuse, or `None` when the
/// admission would succeed.
fn preadmit_conflict_check(
    router: &PluginRouter,
    manifest: &Manifest,
    shelf: &str,
) -> Option<String> {
    // Prefer the stocking matching this shelf — that's where the
    // role + verb set lives in the new manifest form.
    if let Some(stocking) = manifest.stockings.iter().find(|s| s.shelf == shelf)
    {
        return router.would_conflict_with_admission(
            shelf,
            stocking.role,
            &stocking.request_types,
        );
    }
    // Legacy `[target]`-only manifests: derive the role from
    // kind.interaction; the verb set from
    // capabilities.respondent.request_types.
    let role = match manifest.kind.as_ref() {
        Some(k) => StockingRole::from_interaction(k.interaction),
        None => StockingRole::Respondent,
    };
    let verbs: Vec<String> = manifest
        .capabilities
        .respondent
        .as_ref()
        .map(|r| r.request_types.clone())
        .unwrap_or_default();
    router.would_conflict_with_admission(shelf, role, &verbs)
}
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

/// Refuse functional admission of a manifest that declares a UI
/// artefact kind.
///
/// The functional admission paths (`admit_singleton_respondent`,
/// `admit_singleton_warden`, `admit_factory_respondent`,
/// `admit_factory_warden`, the OOP variants, and
/// `admit_out_of_process_from_directory`) accept only
/// [`ArtefactKind::Functional`] manifests — code that loads and
/// responds to verbs. UI artefact kinds
/// ([`ArtefactKind::Theme`] / [`ArtefactKind::UiShell`] /
/// [`ArtefactKind::WidgetKindPack`]) flow through dedicated
/// artefact admission paths added in the active-selection
/// substrate; reaching a functional path with an artefact
/// manifest is an operator authoring error and is refused with a
/// structured diagnostic naming the offending kind.
///
/// Returns `Ok(())` for functional manifests and a
/// [`StewardError::Admission`] for every other kind.
fn refuse_artefact_kind(
    manifest: &Manifest,
    entry_point: &str,
) -> Result<(), StewardError> {
    if manifest.plugin.kind == ArtefactKind::Functional {
        Ok(())
    } else {
        Err(StewardError::Admission(format!(
            "{}: {entry_point} only admits functional plugins; \
             manifest declares plugin.kind = {:?}",
            manifest.plugin.name, manifest.plugin.kind
        )))
    }
}

/// Compulsory admission-time OS-dependency parity gate.
///
/// Reads `privileges.yaml` from `plugin_dir`, runs the SDK's
/// [`enforce_os_dependency_parity`] check, and maps any failure
/// to [`StewardError::Admission`]. Absent or malformed
/// `privileges.yaml` is HARD REFUSE: every functional bundle
/// admitted from disk MUST ship its privileges contract per the
/// packaging contract.
///
/// Called by every disk-backed admission entry point BEFORE any
/// process spawn or wire-op work. Symmetric with the tool-side
/// check in `evo-plugin-tool install` so a bundle that clears
/// install cannot fail admission for the same reason (and vice
/// versa).
///
/// [`enforce_os_dependency_parity`]:
/// evo_plugin_sdk::privileges::enforce_os_dependency_parity
fn enforce_privileges_parity_from_disk(
    plugin_dir: &Path,
    plugin_name: &str,
) -> Result<(), StewardError> {
    use evo_plugin_sdk::privileges::{
        enforce_os_dependency_parity, PrivilegesV1,
    };
    let yaml_path = plugin_dir.join("privileges.yaml");
    let yaml = std::fs::read_to_string(&yaml_path).map_err(|e| {
        StewardError::Admission(format!(
            "{plugin_name}: read privileges.yaml at {}: {e} — every functional plugin bundle MUST ship a privileges contract",
            yaml_path.display()
        ))
    })?;
    let record = PrivilegesV1::from_yaml(&yaml).map_err(|e| {
        StewardError::Admission(format!(
            "{plugin_name}: parse privileges.yaml at {}: {e}",
            yaml_path.display()
        ))
    })?;
    record.validate().map_err(|e| {
        StewardError::Admission(format!(
            "{plugin_name}: privileges.yaml at {} failed semantic validation: {e}",
            yaml_path.display()
        ))
    })?;
    enforce_os_dependency_parity(&record).map_err(|failure| {
        StewardError::Admission(format!("{plugin_name}: {failure}"))
    })?;
    Ok(())
}

/// Invoke `Plugin::load` on the given handle, emitting a debug-entry
/// log line, calling the plugin verb, and emitting a debug-return
/// line carrying `duration_ms` + outcome before mapping any error to
/// [`StewardError::Admission`]. Centralises the lifecycle-verb debug
/// shape per `docs/engineering/LOGGING.md` §2 ("each verb invocation"
/// fires at debug). One helper used at every call site so the shape
/// is identical across the engine, and so future fields (span ids,
/// trace ids) attach in one place.
async fn invoke_plugin_load(
    handle: &mut AdmittedHandle,
    ctx: &mut LoadContext,
    plugin_name: &str,
) -> Result<Option<reprobe::ReprobeTask>, StewardError> {
    // Install the PPAG resolution map + spawn the hot-
    // tightening re-probe task BEFORE `Plugin::load` runs.
    // The plugin's `load` body reads `ctx.capabilities` (and
    // optionally subscribes to `ctx.capabilities_watch`) so
    // both must be stamped first.
    let mut task = reprobe::install_ppag_with_watch(handle, ctx, plugin_name);

    tracing::debug!(
        plugin = %plugin_name,
        verb = "load",
        "plugin lifecycle verb invoking"
    );
    let start = Instant::now();
    let result = handle.load(ctx).await;
    tracing::debug!(
        plugin = %plugin_name,
        verb = "load",
        duration_ms = start.elapsed().as_millis() as u64,
        outcome = if result.is_ok() { "ok" } else { "err" },
        "plugin lifecycle verb returned"
    );
    match result {
        Ok(()) => Ok(task.take()),
        Err(e) => {
            // Load failed; tear the re-probe task down so we
            // do not leak it. The plugin never admitted, so
            // there is no entry to keep the task alive against.
            if let Some(t) = task.take() {
                t.shutdown().await;
            }
            Err(StewardError::Admission(format!(
                "{plugin_name}: load failed: {e}"
            )))
        }
    }
}

/// The admission engine.
///
/// Holds admitted plugins keyed by fully-qualified shelf name. v0 permits
/// one plugin per shelf; additional admissions on the same shelf fail.
///
/// Acquires every shared store handle (subject registry, relation graph,
/// custody ledger, happenings bus, admin audit ledger) plus the
/// catalogue from a single [`Arc<StewardState>`](StewardState) supplied
/// at construction. Plugins receive `SubjectAnnouncer` and
/// `RelationAnnouncer` handles in their `LoadContext` that write to
/// those stores tagged with the plugin's name.
///
/// The routing table itself lives on a [`PluginRouter`] held behind
/// an `Arc`. The engine writes to the router during admission and
/// drain; the dispatch verbs delegate to the router. The server
/// can take its own `Arc<PluginRouter>` clone (via
/// [`AdmissionEngine::router`]) to dispatch without acquiring the
/// engine mutex in a follow-up pass.
pub struct AdmissionEngine {
    /// Routing table plus dispatch primitives. Shared with the
    /// server so dispatch can move off the engine mutex without a
    /// further refactor.
    router: Arc<PluginRouter>,
    /// Shared steward stores plus the catalogue. Built once at boot
    /// and shared with the server, projection engine, and any future
    /// admin paths so dispatch does not serialise on the engine
    /// mutex for store reads.
    state: Arc<StewardState>,
    /// Root for per-plugin `state/` and `credentials/` (see
    /// `PLUGIN_PACKAGING.md`: `/var/lib/evo/plugins/<name>/...`).
    plugin_data_root: PathBuf,
    /// Per-plugin operator config drop-in directory. The engine
    /// looks for `<this>/<plugin_name>.toml` at admission time and
    /// merges its parsed contents into the plugin's
    /// `LoadContext.config`. A missing file is not an error; a
    /// malformed file aborts admission of that plugin.
    plugins_config_dir: PathBuf,
    /// Optional: signature and revocation state for disk-bundled
    /// out-of-process plugins. `None` skips trust checks (harnesses).
    plugin_trust: Option<Arc<PluginTrustState>>,
    /// Optional per-class Unix UID/GID for out-of-process spawns (see
    /// [`StewardConfig`](crate::config::StewardConfig) `[plugins.security]`).
    /// Default: disabled. Ignored on non-Unix.
    plugins_security: PluginsSecurityConfig,
    /// Per-plugin instance announcers, indexed by canonical plugin
    /// name. Populated at admit time for factory-shaped plugins
    /// only; consulted at unload time so the drain path can retract
    /// every announced instance with a structured happening per
    /// instance (see `RegistryInstanceAnnouncer::retract_all_for_drain`).
    /// Singleton plugins are not in the map.
    factory_announcers:
        std::sync::Mutex<HashMap<String, Arc<RegistryInstanceAnnouncer>>>,
    /// Provenance map: plugin canonical name → directory that
    /// admission was driven from. Populated by
    /// [`Self::admit_out_of_process_from_directory`]; consulted by
    /// [`Self::reload_plugin`] to decide whether the plugin can
    /// be re-admitted from its own data without the caller
    /// supplying a fresh handle. Plugins admitted via the typed
    /// `admit_*` entry points (in-process, programmatic OOP)
    /// have no entry here; reload refuses them with a structured
    /// error pointing at unload + admit.
    plugin_origins: std::sync::Mutex<HashMap<String, PathBuf>>,
    /// Prompt ledger handle. Populated via
    /// [`Self::with_prompt_ledger`]; the engine stamps each
    /// out-of-process [`crate::wire_client::WireRespondent`] /
    /// [`crate::wire_client::WireWarden`] with a clone after
    /// `connect` so the EventSink built at `load` time routes
    /// `RequestUserInteraction` frames into the ledger.
    prompt_ledger: Option<Arc<crate::prompts::PromptLedger>>,
    /// Appointments runtime handle. Populated via
    /// [`Self::with_appointments`]; the engine stamps each
    /// in-process plugin's [`LoadContext`] with a router-backed
    /// [`crate::context::RouterAppointmentScheduler`] when its
    /// manifest declares `capabilities.appointments = true`.
    /// Out-of-process plugins reach the runtime via the same
    /// adapter once the wire-side surface lands.
    appointments_runtime: Option<Arc<crate::appointments::AppointmentRuntime>>,
    /// Watches runtime handle. Populated via
    /// [`Self::with_watches`]; the engine stamps each
    /// in-process plugin's [`LoadContext`] with a router-backed
    /// [`crate::context::RouterWatchScheduler`] when its
    /// manifest declares `capabilities.watches = true`.
    watches_runtime: Option<Arc<crate::watches::WatchRuntime>>,
    /// Stream coordinator handle. Populated via
    /// [`Self::with_stream_coordinator`]; the engine stamps each
    /// in-process plugin's [`LoadContext`] with a coordinator-
    /// backed [`crate::context::CoordinatorStreamHost`] when its
    /// manifest declares `capabilities.streams = true`.
    /// Out-of-process plugins reach the coordinator through the
    /// wire layer once the wire-op handler lands.
    stream_coordinator: Option<Arc<crate::streams::StreamCoordinator>>,
    /// Metadata chain handle. Populated via
    /// [`Self::with_metadata_chain`]; the engine stamps each
    /// in-process plugin's [`LoadContext`] with a chain-backed
    /// [`crate::context::ChainMetadataConsumer`] when its manifest
    /// declares `capabilities.metadata = true`. Out-of-process
    /// plugins reach the chain through the wire layer once the
    /// wire-op handler lands.
    metadata_chain: Option<Arc<crate::metadata::MetadataChain>>,
    /// Scheduler-runtime handle. Populated via
    /// [`Self::with_scheduler`]; the engine stamps each in-process
    /// plugin's [`LoadContext`] with a router-backed
    /// [`crate::context::RouterScheduler`] when its manifest
    /// declares `capabilities.scheduler = true`. Out-of-process
    /// plugins reach the runtime through the wire layer once the
    /// wire-op handler lands.
    scheduler_runtime: Option<Arc<crate::scheduler::SchedulerRuntime>>,
    /// Audit-grade ledger handle. Populated via
    /// [`Self::with_ledger`]; consumed alongside the happenings
    /// bus by each in-process LoadContext wrapper
    /// (`CoordinatorStreamHost`, `ChainMetadataConsumer`) so
    /// per-call lifecycle events land
    /// on both observability planes (live happenings stream,
    /// signed forensic record) automatically. Wrappers built
    /// without this handle are no-ops on the ledger plane and
    /// log only via tracing.
    ledger: Option<Arc<crate::ledger::LedgerPrimitive>>,
    /// Plugin runtime directory used by `reload_plugin` to mint
    /// successor sockets during OOP Live reloads. Populated via
    /// [`Self::with_plugin_runtime_dir`]; engines built without
    /// it refuse `reload_plugin` with a structured error so the
    /// configuration omission surfaces loudly rather than
    /// silently disabling the verb.
    plugin_runtime_dir: Option<PathBuf>,
    /// URI-scheme registry handle. Populated via
    /// [`Self::with_uri_schemes`]; consulted at admission time
    /// to register every URI scheme declared by a source plugin's
    /// `[capabilities.source].uri_schemes` block, and at unload
    /// time to unregister them. Engines built without this handle
    /// silently skip URI-scheme registration — supported only for
    /// in-process test harnesses that do not exercise the source-
    /// plugin admission path. Production boot wires the registry
    /// in unconditionally.
    uri_schemes: Option<Arc<UriSchemeRegistry>>,
    /// Per-capability grant revocation store. Populated via
    /// [`Self::with_capability_grant_store`]; consulted at every
    /// admission entry point to compute the operator-revoked
    /// capability set for the plugin, which `build_load_context`
    /// then uses to suppress the corresponding LoadContext handles
    /// regardless of the manifest's per-capability flag. Engines
    /// built without this handle treat every plugin as having an
    /// empty revoked set (the supplied store is the single source
    /// of revocation truth — none configured implies no revocation
    /// authority).
    capability_grant_store:
        Option<Arc<crate::capability_grant::CapabilityGrantStore>>,
    /// Audio routing runtime handle. Populated via
    /// [`Self::with_audio_routing`]; the engine stamps each
    /// audio-capable plugin's [`LoadContext`] with a per-plugin
    /// [`crate::audio_routing::RouterAudioRouting`] handle so
    /// the plugin can fetch the OS-native endpoint the
    /// framework configured for its chain stage. Engines built
    /// without this handle leave the LoadContext field `None`
    /// for every plugin — useful for in-process test harnesses
    /// that do not exercise the audio data plane.
    audio_routing_runtime:
        Option<Arc<crate::audio_routing::AudioRoutingRuntime>>,

    /// Optional audio-plane runtime handle. Populated at boot
    /// via [`Self::with_audio_plane`]; the engine threads it
    /// into `LoadContext::audio_plane` for plugins whose
    /// manifest declares `capabilities.audio_plane = true`.
    /// Engines built without it leave `LoadContext::audio_plane`
    /// at `None` for every plugin — useful for in-process test
    /// harnesses that do not exercise multi-room fan-out.
    audio_plane_runtime: Option<Arc<crate::audio_plane::AudioPlaneRuntime>>,

    /// Optional group store handle. Required alongside
    /// `audio_plane_runtime` so the SDK-side
    /// `AudioPlaneHandle::upsert_group` method can reach the
    /// same store the runtime fans frames out against. Source-
    /// host plugins call `upsert_group` during load to
    /// instantiate the group their TOML declares.
    group_store: Option<Arc<crate::groups::GroupStore>>,

    /// Optional multi-room substrate adapter. Constructed at
    /// boot from `GroupStore + RoleStore` and threaded into
    /// every plugin's `LoadContext.multiroom_substrate`.
    /// Plugins declaring `lifecycle.mode = "reactive-only"`
    /// consume the handle to subscribe to operator gestures
    /// without re-reading TOML.
    multiroom_substrate: Option<
        Arc<dyn evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle>,
    >,

    /// Optional framework asset cache. Populated at boot per
    /// [`Self::with_asset_cache`]; the engine threads the
    /// handle through every plugin's `LoadContext.asset_cache`
    /// so plugins (multi-room artwork propagation, browse-tree
    /// art, etc.) compose against the same content-addressed
    /// store. Absent leaves `asset_cache` at `None` for every
    /// plugin; plugins fall back to placeholder rendering per
    /// the universal artwork-first-or-icon rule.
    asset_cache:
        Option<Arc<dyn evo_plugin_sdk::contract::asset_cache::AssetCache>>,

    /// Optional shelf-request dispatcher. Populated at boot per
    /// [`Self::with_shelf_request_dispatcher`]; the engine
    /// threads the handle through every plugin's
    /// `LoadContext.shelf_request_dispatcher` so plugins can
    /// invoke verbs on shelves owned by other plugins through
    /// the same dispatch_request machinery the wire-op layer
    /// uses. Absent leaves the field at `None`; plugins fall
    /// back to local-only resolution paths per the trait's
    /// contract.
    shelf_request_dispatcher:
        Option<Arc<dyn evo_plugin_sdk::contract::ShelfRequestDispatcher>>,

    /// Optional credential vault handle. Populated at boot per
    /// [`Self::with_credential_vault`]; the engine threads a
    /// per-plugin-scoped handle into every plugin's
    /// `LoadContext.credential_vault` so plugins read + write
    /// operator-supplied secrets (API keys, service passwords,
    /// OAuth tokens) through the framework's single credential
    /// substrate. Absent leaves the field at `None`; plugins that
    /// need credentials fall back to the pre-substrate config /
    /// file paths their crates still ship.
    credential_vault: Option<Arc<crate::credentials::CredentialVault>>,

    /// Optional gateway plugin registry. Populated at boot
    /// per [`Self::with_gateway_registry`]; the engine
    /// registers each admitted plugin whose manifest
    /// declares `[capabilities.gateway]` so the operator
    /// surface lists every gateway plugin currently
    /// resident.
    gateway_registry: Option<Arc<crate::gateway::GatewayRegistry>>,
    /// Optional UI shelf registry. Populated at boot per
    /// [`Self::with_ui_shelves`]; the engine consults the
    /// registry at admission to validate each plugin's UI
    /// stockings (convergence default + explicit
    /// `[[ui.stocks]]`) against the declared shelf
    /// contracts. Without this handle the engine skips the
    /// UI admission gate entirely (test harnesses + early
    /// boot scenarios with no UI surface yet).
    ui_shelves: Option<Arc<crate::ui_registry::ShelfRegistry>>,
    /// Optional UI widget-kind registry. Same lifecycle as
    /// `ui_shelves` — both are required for the UI admission
    /// gate to fire.
    ui_widgets: Option<Arc<crate::ui_registry::WidgetKindRegistry>>,
    /// Optional admitted-stockings store. Same lifecycle as
    /// `ui_shelves` and `ui_widgets`. The engine records the
    /// canonical admitted set per plugin; cardinality checks
    /// at admission consult the store for existing
    /// stockings on each shelf, and operator surfaces
    /// (sub-primitive G) read the store for UI listings.
    ui_admitted: Option<Arc<crate::ui_registry::AdmittedStockingsStore>>,
    /// Optional theme registry. Populated at boot per
    /// [`Self::with_ui_themes`]; the engine consults +
    /// updates the registry on every theme-kind plugin
    /// admission. Without this handle the engine refuses
    /// theme admission (test harnesses without a UI surface
    /// can omit this and the engine simply has nothing to
    /// admit themes against).
    ui_themes: Option<Arc<crate::ui_registry::ThemeRegistry>>,
    /// Optional UI shell registry. Same lifecycle pattern as
    /// `ui_themes`.
    ui_shells: Option<Arc<crate::ui_registry::UiShellRegistry>>,
    /// Optional widget-kind-pack registry. Tracks the
    /// metadata + a11y declarations of every admitted pack;
    /// the kinds the pack provides are folded into
    /// [`Self::ui_widgets`] at admission and rolled back on
    /// unadmission via this registry's pack → kind-id
    /// mapping.
    ui_widget_packs: Option<Arc<crate::ui_registry::WidgetKindPackRegistry>>,
    /// Optional active UI selection runtime. Used by the
    /// artefact unadmit paths to auto-clear the active slot
    /// when the unadmitted artefact is the currently-active
    /// one. Without this handle, unadmitting an active
    /// artefact leaves the active-selection runtime
    /// pointing at a stale plugin name; the shipped boot
    /// path always wires this so the auto-clear is in
    /// force.
    ui_active: Option<Arc<crate::ui_active::ActiveUiSelection>>,
}

impl std::fmt::Debug for AdmissionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionEngine")
            .field("plugin_count", &self.router.len())
            .field("admission_order", &self.router.admission_order())
            .finish()
    }
}

/// Kebab-case label for a `LifecycleMode`, matching the wire
/// vocabulary the manifest declares and the
/// `Happening::PluginReloadDispatched` `mode` field carries.
fn lifecycle_mode_label(
    mode: evo_plugin_sdk::manifest::LifecycleMode,
) -> &'static str {
    match mode {
        evo_plugin_sdk::manifest::LifecycleMode::Frozen => "frozen",
        evo_plugin_sdk::manifest::LifecycleMode::ReactiveOnly => {
            "reactive-only"
        }
        evo_plugin_sdk::manifest::LifecycleMode::ReloadCleanable => {
            "reload-cleanable"
        }
    }
}

impl AdmissionEngine {
    /// Construct an admission engine over a shared
    /// [`StewardState`].
    ///
    /// `state` is the bag of shared store handles plus the catalogue
    /// the steward administers; the engine clones the `Arc` and
    /// reaches into it for every store-touching path. `plugin_data_root`
    /// is the root under which per-plugin `state/` and `credentials/`
    /// paths are built (`/var/lib/evo/plugins/<name>/...` in
    /// production; a tempdir in tests). `plugin_trust` is optional
    /// signature and revocation state for on-disk out-of-process
    /// admission; passing `None` skips signature checks (test
    /// harnesses). `plugins_security` carries optional per-trust-class
    /// Unix UID/GID for out-of-process spawns; the default is the
    /// no-op (every plugin process runs as the steward user).
    pub fn new(
        state: Arc<StewardState>,
        plugin_data_root: PathBuf,
        plugins_config_dir: PathBuf,
        plugin_trust: Option<Arc<PluginTrustState>>,
        plugins_security: PluginsSecurityConfig,
    ) -> Self {
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        Self {
            router,
            state,
            plugin_data_root,
            plugins_config_dir,
            plugin_trust,
            plugins_security,
            factory_announcers: std::sync::Mutex::new(HashMap::new()),
            plugin_origins: std::sync::Mutex::new(HashMap::new()),
            prompt_ledger: None,
            appointments_runtime: None,
            watches_runtime: None,
            stream_coordinator: None,
            metadata_chain: None,
            scheduler_runtime: None,
            ledger: None,
            plugin_runtime_dir: None,
            uri_schemes: None,
            capability_grant_store: None,
            audio_routing_runtime: None,
            audio_plane_runtime: None,
            group_store: None,
            multiroom_substrate: None,
            asset_cache: None,
            shelf_request_dispatcher: None,
            credential_vault: None,
            gateway_registry: None,
            ui_shelves: None,
            ui_widgets: None,
            ui_admitted: None,
            ui_themes: None,
            ui_shells: None,
            ui_widget_packs: None,
            ui_active: None,
        }
    }

    /// Compute the transitive set of admitted plugins whose
    /// manifests list `target_plugin` as a required dependency.
    /// Performs a BFS from the target, collecting every plugin
    /// that would be broken by disabling the target — including
    /// dependents-of-dependents. Returns names in BFS-discovery
    /// order so operator surfaces can show the impact graph
    /// "outermost first".
    ///
    /// Plugins admitted without a manifest (legacy in-process
    /// admission paths) cannot declare dependencies and never
    /// appear in the result.
    ///
    /// The result EXCLUDES `target_plugin` itself; it lists
    /// only the dependents.
    pub fn transitive_required_dependents(
        &self,
        target_plugin: &str,
    ) -> Vec<String> {
        // Collect (admitted_name -> required-list) for every
        // admitted plugin that has a manifest. Reading manifests
        // once up front avoids re-locking each entry's manifest
        // mutex per BFS step.
        let by_name = self.required_dependency_index();

        let mut visited: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> =
            std::collections::VecDeque::new();
        let mut out: Vec<String> = Vec::new();

        // Seed BFS with direct dependents of the target.
        for (name, requires) in &by_name {
            if requires.contains(target_plugin) && !visited.contains(name) {
                visited.insert(name.clone());
                queue.push_back(name.clone());
                out.push(name.clone());
            }
        }
        while let Some(current) = queue.pop_front() {
            for (name, requires) in &by_name {
                if requires.contains(&current) && !visited.contains(name) {
                    visited.insert(name.clone());
                    queue.push_back(name.clone());
                    out.push(name.clone());
                }
            }
        }
        out
    }

    /// Build a map of `admitted_name -> set-of-required-deps`
    /// from the router's admitted plugins, reading each entry's
    /// manifest once. Used by
    /// [`Self::transitive_required_dependents`].
    fn required_dependency_index(
        &self,
    ) -> std::collections::HashMap<String, std::collections::HashSet<String>>
    {
        let mut by_name = std::collections::HashMap::new();
        for entry in self.router.entries_in_order() {
            let manifest_guard =
                entry.manifest.lock().expect("manifest mutex poisoned");
            let Some(manifest) = manifest_guard.as_ref() else {
                continue;
            };
            let required: std::collections::HashSet<String> = manifest
                .dependencies
                .required
                .iter()
                .map(|d| d.plugin_name().to_string())
                .collect();
            by_name.insert(entry.name.clone(), required);
        }
        by_name
    }

    /// Read the recorded admission origin (bundle directory) for
    /// a plugin admitted via
    /// [`Self::admit_out_of_process_from_directory`]. Returns
    /// `None` for plugins admitted via the typed in-process /
    /// programmatic OOP entry points (no recorded directory) or
    /// for plugins not yet admitted. Used by the server-side
    /// lifecycle-policy gate to classify a plugin's distribution
    /// model (bundled / admitted) by path-prefix matching.
    pub fn plugin_origin(&self, plugin_name: &str) -> Option<PathBuf> {
        self.plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .get(plugin_name)
            .cloned()
    }

    /// Builder-style setter for the plugin runtime directory.
    /// Required by [`Self::reload_plugin`] for OOP Live reloads
    /// (each successor process mints a fresh socket under this
    /// directory). Engines built without it refuse the verb with
    /// a structured error.
    pub fn with_plugin_runtime_dir(mut self, dir: PathBuf) -> Self {
        self.plugin_runtime_dir = Some(dir);
        self
    }

    /// Builder-style setter for the watches-runtime handle.
    /// Plugins whose manifest declares
    /// `capabilities.watches = true` get a router-backed
    /// scheduler stamped on their [`LoadContext`] only when
    /// this handle is set.
    pub fn with_watches(
        mut self,
        runtime: Arc<crate::watches::WatchRuntime>,
    ) -> Self {
        self.watches_runtime = Some(runtime);
        self
    }

    /// Builder-style setter for the appointments-runtime handle.
    /// Plugins whose manifest declares
    /// `capabilities.appointments = true` get a router-backed
    /// scheduler stamped on their [`LoadContext`] only when this
    /// handle is set. Plugins admitted before
    /// `with_appointments` is called have no scheduler on their
    /// LoadContext and unwrap-on-call surfaces the
    /// configuration omission loudly.
    pub fn with_appointments(
        mut self,
        runtime: Arc<crate::appointments::AppointmentRuntime>,
    ) -> Self {
        self.appointments_runtime = Some(runtime);
        self
    }

    /// Builder-style setter for the stream-coordinator handle.
    /// Plugins whose manifest declares
    /// `capabilities.streams = true` get a coordinator-backed
    /// host stamped on their [`LoadContext`] only when this
    /// handle is set. Plugins admitted before
    /// `with_stream_coordinator` is called have no host on their
    /// LoadContext and unwrap-on-call surfaces the configuration
    /// omission loudly.
    pub fn with_stream_coordinator(
        mut self,
        coordinator: Arc<crate::streams::StreamCoordinator>,
    ) -> Self {
        self.stream_coordinator = Some(coordinator);
        self
    }

    /// Builder-style setter for the metadata-chain handle.
    /// Plugins whose manifest declares
    /// `capabilities.metadata = true` get a chain-backed consumer
    /// stamped on their [`LoadContext`] only when this handle is
    /// set. Plugins admitted before `with_metadata_chain` is
    /// called have no consumer on their LoadContext and unwrap-on-
    /// call surfaces the configuration omission loudly.
    pub fn with_metadata_chain(
        mut self,
        chain: Arc<crate::metadata::MetadataChain>,
    ) -> Self {
        self.metadata_chain = Some(chain);
        self
    }

    /// Builder-style setter for the scheduler-runtime handle.
    /// Plugins whose manifest declares
    /// `capabilities.scheduler = true` get a router-backed
    /// [`crate::context::RouterScheduler`] stamped on their
    /// [`LoadContext`] only when this handle is set. Plugins
    /// admitted before `with_scheduler` is called have no
    /// scheduler on their LoadContext and unwrap-on-call surfaces
    /// the configuration omission loudly.
    pub fn with_scheduler(
        mut self,
        runtime: Arc<crate::scheduler::SchedulerRuntime>,
    ) -> Self {
        self.scheduler_runtime = Some(runtime);
        self
    }

    /// Builder-style setter for the audit-grade ledger handle.
    /// When set, in-process LoadContext wrappers
    /// (`CoordinatorStreamHost`, `ChainMetadataConsumer`) write
    /// per-call lifecycle events to
    /// both the happenings bus (live observability) and the ledger
    /// (signed forensic record). When unset, lifecycle events skip
    /// the ledger plane (the wrapper falls back to happenings-only
    /// when the bus is set; to no-op when neither is set).
    pub fn with_ledger(
        mut self,
        ledger: Arc<crate::ledger::LedgerPrimitive>,
    ) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Builder-style setter for the prompt ledger handle. The
    /// engine stamps it onto each [`crate::wire_client::WireRespondent`]
    /// / [`crate::wire_client::WireWarden`] adapter at admission
    /// time. Plugins admitted before `with_prompt_ledger` is
    /// called have no ledger handle on their EventSink and
    /// surface a structured `prompt_ledger_not_configured`
    /// error on every `RequestUserInteraction` frame; this is
    /// loud-by-design.
    pub fn with_prompt_ledger(
        mut self,
        ledger: Arc<crate::prompts::PromptLedger>,
    ) -> Self {
        self.prompt_ledger = Some(ledger);
        self
    }

    /// Builder-style setter for the URI-scheme registry handle.
    /// When set, admission of a source plugin (manifest with a
    /// non-empty `[capabilities.source].uri_schemes` list)
    /// registers each declared scheme in the registry, refusing
    /// admission with a structured error when another admitted
    /// plugin already owns the scheme. Unload, disable, and
    /// uninstall release the schemes back to the registry. When
    /// unset, source-plugin admission silently skips registration
    /// — supported only for unit-test harnesses that do not
    /// exercise URI-scheme dispatch.
    pub fn with_uri_schemes(
        mut self,
        registry: Arc<UriSchemeRegistry>,
    ) -> Self {
        self.uri_schemes = Some(registry);
        self
    }

    /// Builder-style setter for the per-capability grant
    /// revocation store. When set, every admission entry point
    /// consults the store and passes the revoked-capability set
    /// for the plugin into [`build_load_context`], which then
    /// suppresses the corresponding LoadContext handles regardless
    /// of the manifest's per-capability flag. When unset, every
    /// plugin is treated as having an empty revoked set —
    /// supported for in-process test harnesses that do not exercise
    /// the operator revocation surface.
    pub fn with_capability_grant_store(
        mut self,
        store: Arc<crate::capability_grant::CapabilityGrantStore>,
    ) -> Self {
        self.capability_grant_store = Some(store);
        self
    }

    /// Builder-style setter for the audio-routing runtime. When
    /// set, the engine stamps each audio-capable plugin's
    /// [`LoadContext`] with a per-plugin
    /// [`crate::audio_routing::RouterAudioRouting`] handle so
    /// the plugin can fetch the OS-native endpoint the framework
    /// configured for its chain stage. When unset, every plugin
    /// sees `None` for `LoadContext.audio_routing` — supported
    /// for in-process test harnesses that do not exercise the
    /// audio data plane.
    pub fn with_audio_routing(
        mut self,
        runtime: Arc<crate::audio_routing::AudioRoutingRuntime>,
    ) -> Self {
        self.audio_routing_runtime = Some(runtime);
        self
    }

    /// Plumb the multi-room audio-plane runtime the engine
    /// threads into `LoadContext::audio_plane` for plugins
    /// whose manifest declares `capabilities.audio_plane = true`.
    /// Engines built without the runtime see `None` for every
    /// plugin — supported for in-process test harnesses that
    /// do not exercise multi-room fan-out.
    pub fn with_audio_plane(
        mut self,
        runtime: Arc<crate::audio_plane::AudioPlaneRuntime>,
    ) -> Self {
        self.audio_plane_runtime = Some(runtime);
        self
    }

    /// Plumb the framework's content-addressed asset cache
    /// the engine threads into every plugin's
    /// `LoadContext.asset_cache`. Engines built without the
    /// cache leave `asset_cache` at `None` for every plugin;
    /// plugins fall back to placeholder rendering per the
    /// universal artwork-first-or-icon rule when the handle
    /// is absent.
    pub fn with_asset_cache(
        mut self,
        cache: Arc<dyn evo_plugin_sdk::contract::asset_cache::AssetCache>,
    ) -> Self {
        self.asset_cache = Some(cache);
        self
    }

    /// Plumb the framework's plugin-to-plugin shelf request
    /// dispatcher the engine threads into every plugin's
    /// `LoadContext.shelf_request_dispatcher`. Engines built
    /// without the dispatcher leave the field at `None` for
    /// every plugin; plugins gracefully fall back to local-only
    /// resolution paths per the trait's contract.
    ///
    /// The dispatcher is constructed at the steward level
    /// (it holds the request router + state handles); the
    /// engine receives the prepared instance and shares it
    /// across every admission via Arc.
    pub fn with_shelf_request_dispatcher(
        mut self,
        dispatcher: Arc<dyn evo_plugin_sdk::contract::ShelfRequestDispatcher>,
    ) -> Self {
        self.shelf_request_dispatcher = Some(dispatcher);
        self
    }

    /// Plumb the framework credential vault the engine binds into
    /// every plugin's `LoadContext.credential_vault` — one
    /// `PluginScopedCredentialVault` per admission, canonical plugin
    /// id closed over so plugins physically cannot address any
    /// other plugin's rows. Engines built without the vault leave
    /// the field `None` for every plugin; plugins that need
    /// operator-supplied credentials fall back to the file-path
    /// / config-key paths the pre-substrate plugins still ship.
    pub fn with_credential_vault(
        mut self,
        vault: Arc<crate::credentials::CredentialVault>,
    ) -> Self {
        self.credential_vault = Some(vault);
        self
    }

    /// Plumb the group store the framework consults during
    /// multi-room fan-out and that source-host plugins
    /// instantiate their group against via the SDK's
    /// `AudioPlaneHandle::upsert_group`. Should be set
    /// alongside [`Self::with_audio_plane`]; engines that omit
    /// it leave source-host plugins unable to declare their
    /// group from operator config.
    pub fn with_group_store(
        mut self,
        store: Arc<crate::groups::GroupStore>,
    ) -> Self {
        self.group_store = Some(store);
        self
    }

    /// Plumb the multi-room substrate adapter implementing the
    /// SDK's `MultiroomSubstrateHandle` trait. Threaded into
    /// every plugin's `LoadContext.multiroom_substrate` so
    /// reactive-only plugins (the multi-room plugin first)
    /// subscribe to operator gestures via the SDK contract.
    pub fn with_multiroom_substrate(
        mut self,
        adapter: Arc<
            dyn evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle,
        >,
    ) -> Self {
        self.multiroom_substrate = Some(adapter);
        self
    }

    /// Plumb the gateway plugin registry the engine populates
    /// at admission for every plugin whose manifest declares
    /// `[capabilities.gateway]`. Engines built without a
    /// registry skip the registration step (in-process test
    /// harnesses); production boot wires the registry
    /// unconditionally.
    pub fn with_gateway_registry(
        mut self,
        registry: Arc<crate::gateway::GatewayRegistry>,
    ) -> Self {
        self.gateway_registry = Some(registry);
        self
    }

    /// Register a plugin's gateway declaration if it has one.
    /// Called from the admission paths after manifest
    /// validation succeeds. No-op when the engine has no
    /// gateway registry configured or the manifest does not
    /// declare a gateway.
    pub(crate) async fn register_gateway_if_declared(
        &self,
        plugin_name: &str,
        manifest: &evo_plugin_sdk::manifest::Manifest,
    ) {
        let Some(registry) = &self.gateway_registry else {
            return;
        };
        let Some(gateway) = manifest.capabilities.gateway.as_ref() else {
            return;
        };
        let info = crate::gateway::GatewayInfo {
            plugin_name: plugin_name.to_string(),
            protocol: gateway.protocol.clone(),
            direction: gateway.direction,
            licensed: gateway.licensed,
            registered_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        registry.register(info).await;
    }

    /// Unregister a plugin's gateway declaration. Called from
    /// the admission paths on plugin removal. No-op when the
    /// engine has no gateway registry configured or the
    /// plugin had no gateway declaration.
    pub(crate) async fn unregister_gateway(&self, plugin_name: &str) {
        if let Some(registry) = &self.gateway_registry {
            registry.unregister(plugin_name).await;
        }
    }

    /// Configure the UI shelf registry. The admission gate
    /// validates each plugin's UI stockings against this
    /// registry; absent the handle the gate skips UI
    /// validation entirely (test harnesses + early boot).
    pub fn with_ui_shelves(
        mut self,
        registry: Arc<crate::ui_registry::ShelfRegistry>,
    ) -> Self {
        self.ui_shelves = Some(registry);
        self
    }

    /// Configure the UI widget-kind registry. Required
    /// alongside [`Self::with_ui_shelves`] for the UI
    /// admission gate to fire.
    pub fn with_ui_widgets(
        mut self,
        registry: Arc<crate::ui_registry::WidgetKindRegistry>,
    ) -> Self {
        self.ui_widgets = Some(registry);
        self
    }

    /// Register additional shelf contracts on the configured
    /// UI shelf registry. Distributions invoke this from their
    /// `AdmissionSetup` closure to add Tier 2 reference-device
    /// shelves (e.g. `audio.playback.transport`, `audio.queue`)
    /// alongside the framework's Tier 1 universals, BEFORE any
    /// plugin admission so the admission gate validates
    /// stockings against the combined set.
    ///
    /// Refuses with `not_configured` when no shelf registry is
    /// wired; bubbles registry-level duplicate errors per
    /// [`crate::ui_registry::ShelfRegistry::register`].
    pub async fn register_ui_shelves(
        &self,
        shelves: &[evo_plugin_sdk::ui::ShelfContract],
    ) -> Result<(), StewardError> {
        let registry = self.ui_shelves.as_ref().ok_or_else(|| {
            StewardError::Admission(
                "register_ui_shelves: no shelf registry configured; \
                 call with_ui_shelves before registering Tier 2 shelves"
                    .to_string(),
            )
        })?;
        for shelf in shelves {
            registry.register(shelf.clone()).await.map_err(|e| {
                StewardError::Admission(format!(
                    "register Tier 2 shelf {:?}: {e}",
                    shelf.id
                ))
            })?;
        }
        Ok(())
    }

    /// Register additional widget-kind envelopes on the
    /// configured registry. Symmetric to
    /// [`Self::register_ui_shelves`] — distributions add Tier 2
    /// reference-device widget kinds alongside the framework's
    /// Tier 1 universals before plugin admission.
    pub async fn register_ui_widget_kinds(
        &self,
        kinds: &[evo_plugin_sdk::ui::WidgetKindEnvelope],
    ) -> Result<(), StewardError> {
        let registry = self.ui_widgets.as_ref().ok_or_else(|| {
            StewardError::Admission(
                "register_ui_widget_kinds: no widget-kind registry \
                 configured; call with_ui_widgets before registering Tier 2 \
                 widget kinds"
                    .to_string(),
            )
        })?;
        for kind in kinds {
            registry.register(kind.clone()).await.map_err(|e| {
                StewardError::Admission(format!(
                    "register Tier 2 widget kind {:?}: {e}",
                    kind.id
                ))
            })?;
        }
        Ok(())
    }

    /// Configure the admitted-stockings store. Required
    /// alongside [`Self::with_ui_shelves`] and
    /// [`Self::with_ui_widgets`] for the UI admission gate
    /// to fire.
    pub fn with_ui_admitted(
        mut self,
        store: Arc<crate::ui_registry::AdmittedStockingsStore>,
    ) -> Self {
        self.ui_admitted = Some(store);
        self
    }

    /// Configure the theme registry. Required for
    /// [`Self::admit_theme`] to succeed; without it theme
    /// admission refuses with `Admission(... no theme
    /// registry configured)`.
    pub fn with_ui_themes(
        mut self,
        registry: Arc<crate::ui_registry::ThemeRegistry>,
    ) -> Self {
        self.ui_themes = Some(registry);
        self
    }

    /// Configure the UI shell registry. Required for
    /// [`Self::admit_ui_shell`].
    pub fn with_ui_shells(
        mut self,
        registry: Arc<crate::ui_registry::UiShellRegistry>,
    ) -> Self {
        self.ui_shells = Some(registry);
        self
    }

    /// Configure the widget-kind-pack registry. Required
    /// alongside [`Self::with_ui_widgets`] for
    /// [`Self::admit_widget_kind_pack`]: the kinds the pack
    /// provides land in the widget-kind registry, and the
    /// pack's metadata + a11y declarations land here so
    /// unadmission can roll the kinds back.
    pub fn with_ui_widget_packs(
        mut self,
        registry: Arc<crate::ui_registry::WidgetKindPackRegistry>,
    ) -> Self {
        self.ui_widget_packs = Some(registry);
        self
    }

    /// Configure the active UI selection runtime. Used by
    /// the artefact unadmit paths to auto-clear the active
    /// slot when the unadmitted artefact is the currently-
    /// active one. The shipped boot path wires this from
    /// the same runtime the wire ops `activate_theme` /
    /// `activate_ui_shell` consult.
    pub fn with_ui_active(
        mut self,
        runtime: Arc<crate::ui_active::ActiveUiSelection>,
    ) -> Self {
        self.ui_active = Some(runtime);
        self
    }

    /// Validate the plugin's UI stockings (convergence
    /// default + explicit `[[ui.stocks]]`) and record the
    /// admitted set against the per-plugin store. Called
    /// from each `admit_*` path after `check_admin_trust` so
    /// every admit path enforces the same UI contract. No-op
    /// when any of the three UI handles is unconfigured —
    /// admission proceeds as if the plugin had no UI
    /// surface (test-harness convenience).
    pub(crate) async fn check_and_record_ui_stockings(
        &self,
        plugin_name: &str,
        manifest: &evo_plugin_sdk::manifest::Manifest,
    ) -> Result<(), StewardError> {
        let (Some(shelves), Some(widgets), Some(admitted)) = (
            self.ui_shelves.as_ref(),
            self.ui_widgets.as_ref(),
            self.ui_admitted.as_ref(),
        ) else {
            return Ok(());
        };
        // Snapshot the prior recording before validation so
        // we can compute per-shelf change deltas after the
        // store has been updated.
        let before = admitted.get(plugin_name).await.unwrap_or_default();
        let after = crate::admission::validation::check_ui_stockings(
            plugin_name,
            manifest,
            shelves,
            widgets,
            admitted,
        )
        .await?;
        emit_ui_shelf_changes(&self.state.bus, plugin_name, &before, &after)
            .await;
        Ok(())
    }

    /// Drop the per-plugin UI stockings recording. Called
    /// from the admission paths on plugin removal. No-op
    /// when the admitted store is unconfigured.
    pub(crate) async fn forget_ui_stockings(&self, plugin_name: &str) {
        let Some(admitted) = &self.ui_admitted else {
            return;
        };
        let before = admitted.get(plugin_name).await.unwrap_or_default();
        admitted.forget(plugin_name).await;
        emit_ui_shelf_changes(&self.state.bus, plugin_name, &before, &[]).await;
    }

    /// Compute the operator-revoked capability set for one
    /// plugin from the configured
    /// [`crate::capability_grant::CapabilityGrantStore`]. Returns
    /// the empty set when the engine was built without a store,
    /// preserving fail-open semantics for in-process test
    /// harnesses while production boot wires the store
    /// unconditionally. A persistence-layer error propagates
    /// through `StewardError::Persistence` so admission fails
    /// closed rather than silently granting a revoked
    /// capability.
    async fn revoked_capabilities_for(
        &self,
        plugin_name: &str,
    ) -> Result<std::collections::HashSet<String>, StewardError> {
        match &self.capability_grant_store {
            Some(store) => store
                .revoked_set_for_plugin(plugin_name)
                .await
                .map_err(|e| match e {
                    crate::capability_grant::CapabilityGrantError::Persistence(p) => {
                        StewardError::Persistence(p)
                    }
                }),
            None => Ok(std::collections::HashSet::new()),
        }
    }

    /// Register every URI scheme declared in the manifest's
    /// `[capabilities.source]` block with the framework's URI
    /// scheme registry. Idempotent re-registration of the same
    /// `(scheme, plugin)` pair succeeds; conflict with a different
    /// plugin returns a structured admission error. No-op when
    /// the manifest has no source capabilities, or when the
    /// engine was constructed without
    /// [`Self::with_uri_schemes`].
    ///
    /// Caller is expected to roll back already-registered schemes
    /// on a partial failure, since the helper iterates linearly
    /// and stops on the first conflict.
    async fn register_uri_schemes_for_manifest(
        &self,
        manifest: &Manifest,
    ) -> Result<(), StewardError> {
        let Some(registry) = self.uri_schemes.as_ref() else {
            return Ok(());
        };
        let Some(source) = manifest.capabilities.source.as_ref() else {
            return Ok(());
        };
        let plugin_name = manifest.plugin.name.as_str();
        let mut registered: Vec<&str> = Vec::new();
        for scheme in &source.uri_schemes {
            match registry.register(scheme, plugin_name).await {
                Ok(()) => registered.push(scheme.as_str()),
                Err(e) => {
                    // Roll back any schemes registered in this
                    // call so a partial admission failure does
                    // not leak ownership rows. Best-effort: log
                    // any unregister errors but bubble the
                    // original conflict.
                    for done in registered {
                        if let Err(unreg_err) = registry.unregister(done).await
                        {
                            tracing::warn!(
                                plugin = plugin_name,
                                scheme = done,
                                error = %unreg_err,
                                "URI-scheme rollback failed during admission"
                            );
                        }
                    }
                    return Err(match e {
                        QueueError::SchemeConflict {
                            scheme,
                            existing_plugin,
                            requested_plugin,
                        } => StewardError::Admission(format!(
                            "{plugin_name}: URI scheme {scheme:?} already \
                             owned by {existing_plugin}; refusing admission \
                             of {requested_plugin}"
                        )),
                        other => StewardError::Admission(format!(
                            "{plugin_name}: URI scheme registration failed \
                             for {scheme:?}: {other}"
                        )),
                    });
                }
            }
        }
        Ok(())
    }

    /// Unregister every URI scheme declared in the manifest's
    /// `[capabilities.source]` block. Best-effort: failures are
    /// logged but do not propagate, since unregister runs from
    /// unload paths whose contract is to make forward progress
    /// even when individual cleanup steps fail. No-op when the
    /// manifest has no source capabilities, or when the engine
    /// was constructed without [`Self::with_uri_schemes`].
    async fn unregister_uri_schemes_for_manifest(&self, manifest: &Manifest) {
        let Some(registry) = self.uri_schemes.as_ref() else {
            return;
        };
        let Some(source) = manifest.capabilities.source.as_ref() else {
            return;
        };
        for scheme in &source.uri_schemes {
            if let Err(e) = registry.unregister(scheme).await {
                tracing::warn!(
                    plugin = %manifest.plugin.name,
                    scheme = scheme,
                    error = %e,
                    "URI-scheme unregister failed"
                );
            }
        }
    }

    /// Unregister every URI scheme owned by the plugin behind
    /// this entry. Reads the manifest off the entry under its
    /// internal mutex; entries with no manifest (test fixtures)
    /// silently skip.
    async fn unregister_uri_schemes_for_entry(&self, entry: &PluginEntry) {
        if self.uri_schemes.is_none() {
            return;
        }
        let manifest = entry
            .manifest
            .lock()
            .expect("plugin entry manifest mutex poisoned")
            .clone();
        if let Some(manifest) = manifest {
            self.unregister_uri_schemes_for_manifest(&manifest).await;
        }
    }

    /// Borrow the [`PluginRouter`] handle this engine constructed.
    ///
    /// The server clones this `Arc` to dispatch requests without
    /// acquiring the engine mutex: concurrent client requests to
    /// different plugins run truly in parallel rather than serialising
    /// on the admission lock.
    pub fn router(&self) -> &Arc<PluginRouter> {
        &self.router
    }

    /// Borrow the shared [`StewardState`] handle this
    /// engine was constructed over.
    ///
    /// Used by tests and by the server / projection layer to reach the
    /// individual stores without going through engine accessors.
    pub fn state(&self) -> &Arc<StewardState> {
        &self.state
    }

    /// Root under which per-plugin `state/` and `credentials/` paths are
    /// built for [`LoadContext`].
    pub fn plugin_data_root(&self) -> &Path {
        &self.plugin_data_root
    }

    /// Borrow a handle to the subject registry used by this engine.
    pub fn registry(&self) -> Arc<SubjectRegistry> {
        Arc::clone(&self.state.subjects)
    }

    /// Borrow a handle to the relation graph used by this engine.
    pub fn relation_graph(&self) -> Arc<RelationGraph> {
        Arc::clone(&self.state.relations)
    }

    /// Borrow a handle to the custody ledger used by this engine.
    pub fn custody_ledger(&self) -> Arc<CustodyLedger> {
        Arc::clone(&self.state.custody)
    }

    /// Borrow a handle to the happenings bus used by this engine.
    pub fn happening_bus(&self) -> Arc<HappeningBus> {
        Arc::clone(&self.state.bus)
    }

    /// Borrow a handle to the admin audit ledger used by this
    /// engine. Tests and the future admin-audit client op use
    /// this to read the recorded entries.
    pub fn admin_ledger(&self) -> Arc<AdminLedger> {
        Arc::clone(&self.state.admin)
    }

    /// Borrow a handle to the framework credential vault the
    /// engine binds into every plugin's LoadContext. Returns
    /// `None` on engines constructed without
    /// [`Self::with_credential_vault`] (test harnesses, admission
    /// benches). The steward's credential wire-op handlers consult
    /// this to reach the underlying vault primitive.
    pub fn credential_vault(
        &self,
    ) -> Option<Arc<crate::credentials::CredentialVault>> {
        self.credential_vault.as_ref().map(Arc::clone)
    }

    /// Borrow a handle to the catalogue this engine validates
    /// admissions against.
    pub fn catalogue(&self) -> Arc<Catalogue> {
        self.state.current_catalogue()
    }

    /// Borrow a handle to the persistence store. The discovery
    /// pass consults this for the operator-disabled set;
    /// operator-issued plugin lifecycle verbs write through it.
    pub fn persistence(&self) -> Arc<dyn crate::persistence::PersistenceStore> {
        Arc::clone(&self.state.persistence)
    }

    /// Number of currently admitted plugins.
    pub fn len(&self) -> usize {
        self.router.len()
    }

    /// True if no plugins are admitted.
    pub fn is_empty(&self) -> bool {
        self.router.is_empty()
    }

    /// Admit an in-process singleton respondent.
    ///
    /// Runs the full admission sequence:
    ///
    /// 1. Validates manifest is internally consistent.
    /// 2. Verifies the target shelf exists in the catalogue.
    /// 3. Verifies no plugin is already admitted on that shelf.
    /// 4. Constructs a [`LoadContext`] and calls the plugin's `load`.
    /// 5. Calls the plugin's `describe` and checks identity matches the
    ///    manifest.
    /// 6. Registers the plugin for request dispatch.
    ///
    /// On any failure the plugin is dropped and an error is returned
    /// naming the specific reason.
    pub async fn admit_singleton_respondent<T>(
        &mut self,
        plugin: T,
        manifest: Manifest,
    ) -> Result<(), StewardError>
    where
        T: Respondent + 'static,
    {
        manifest.validate()?;
        refuse_artefact_kind(&manifest, "admit_singleton_respondent")?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        self.check_and_record_ui_stockings(&manifest.plugin.name, &manifest)
            .await?;
        validation::check_dependencies(&manifest, &self.router)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let catalogue = self.state.current_catalogue();
        let shelf =
            catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
                StewardError::Admission(format!(
                    "{}: target shelf not in catalogue: {}",
                    manifest.plugin.name, shelf_qualified
                ))
            })?;

        if !shelf.accepts_shape(manifest.target.shape) {
            let supports_note = if shelf.shape_supports.is_empty() {
                String::new()
            } else {
                format!(" (also accepts {:?})", shelf.shape_supports)
            };
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}{}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape,
                supports_note
            )));
        }

        if let Some(conflict) =
            preadmit_conflict_check(&self.router, &manifest, &shelf_qualified)
        {
            return Err(StewardError::Admission(format!(
                "{}: {conflict}",
                manifest.plugin.name
            )));
        }

        let manifest_kind = manifest.require_kind();
        if manifest_kind.interaction != InteractionShape::Respondent {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_respondent requires kind.interaction \
                 = 'respondent', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.interaction
            )));
        }
        if manifest_kind.instance != InstanceShape::Singleton {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_respondent requires kind.instance \
                 = 'singleton', manifest declares {:?} \
                 (use admit_factory_respondent for factories)",
                manifest.plugin.name, manifest_kind.instance
            )));
        }

        let mut handle = AdmittedHandle::Respondent(Box::new(
            RespondentAdapter::new(plugin),
        ));

        let description = handle.describe().await;
        if description.identity.name != manifest.plugin.name {
            return Err(StewardError::Admission(format!(
                "plugin describe() name {} does not match manifest name {}",
                description.identity.name, manifest.plugin.name
            )));
        }
        if description.identity.version != manifest.plugin.version {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() version {} does not match manifest version {}",
                manifest.plugin.name,
                description.identity.version,
                manifest.plugin.version
            )));
        }
        if description.identity.contract != manifest.plugin.contract {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() contract {} does not match manifest contract {}",
                manifest.plugin.name,
                description.identity.contract,
                manifest.plugin.contract
            )));
        }

        validation::check_drift_and_skew(
            &manifest,
            &description.runtime_capabilities,
            &self.state.bus,
        )
        .await?;

        let revoked =
            self.revoked_capabilities_for(&manifest.plugin.name).await?;
        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            self.state.current_catalogue(),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
            self.appointments_runtime.clone(),
            self.watches_runtime.clone(),
            self.stream_coordinator.clone(),
            self.metadata_chain.clone(),
            self.scheduler_runtime.clone(),
            self.ledger.clone(),
            self.audio_routing_runtime.clone(),
            self.audio_plane_runtime.clone(),
            self.group_store.clone(),
            self.multiroom_substrate.clone(),
            self.asset_cache.clone(),
            self.shelf_request_dispatcher.clone(),
            self.credential_vault.clone(),
            Arc::clone(&self.state.credential_change_bus),
            self.state.online_provider_config_store.get().cloned(),
            Arc::clone(&self.state.online_provider_config_bus),
            &revoked,
        )?;
        // Register URI schemes declared by the source plugin
        // before invoking load, so a scheme conflict refuses
        // admission before we pay the load cost. On load failure,
        // roll back the schemes to release them for retry.
        self.register_uri_schemes_for_manifest(&manifest).await?;
        let reprobe_task = match invoke_plugin_load(
            &mut handle,
            &mut ctx,
            &manifest.plugin.name,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.unregister_uri_schemes_for_manifest(&manifest).await;
                return Err(e);
            }
        };

        let kind_name = handle.kind_name();
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            trust_class = ?manifest.trust.class,
            "plugin admitted"
        );

        let entry = Arc::new(
            PluginEntry::new_with_policy(
                manifest.plugin.name.clone(),
                shelf_qualified.clone(),
                handle,
                EnforcementPolicy::from_manifest(&manifest),
            )
            .with_manifest(Arc::new(manifest.clone()))
            .with_stockings(manifest.stockings.clone())
            .with_reprobe(reprobe_task),
        );
        if let Err(e) = self.router.insert(entry) {
            // The duplicate-shelf check earlier in this method
            // covers the only expected failure mode; reaching
            // here implies a programming error or a late race.
            // Release the URI schemes we registered so the
            // operator can retry without manual cleanup.
            self.unregister_uri_schemes_for_manifest(&manifest).await;
            return Err(e);
        }

        // Record the plugin's version against its claimant token so
        // the `resolve_claimants` op can return both name and
        // version. Token derivation deliberately omits the version
        // (see [`crate::claimant`]), so the issuer needs an explicit
        // record_version call to populate the reverse-lookup row.
        self.state.claimant_issuer.record_version(
            &manifest.plugin.name,
            manifest.plugin.version.to_string(),
        );

        Ok(())
    }

    /// Admit an in-process singleton warden.
    ///
    /// Parallel to [`Self::admit_singleton_respondent`]: runs the same
    /// admission sequence (manifest validation, shelf lookup, shape
    /// match, duplicate check, identity check, load) but stores the
    /// plugin as an [`AdmittedHandle::Warden`].
    ///
    /// Additionally rejects manifests whose `kind.interaction` is not
    /// `warden`. A respondent manifest passed to this method is a
    /// type-system-undetectable misuse; the runtime check catches it.
    pub async fn admit_singleton_warden<T>(
        &mut self,
        plugin: T,
        manifest: Manifest,
    ) -> Result<(), StewardError>
    where
        T: Warden + 'static,
    {
        manifest.validate()?;
        refuse_artefact_kind(&manifest, "admit_singleton_warden")?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        self.check_and_record_ui_stockings(&manifest.plugin.name, &manifest)
            .await?;
        validation::check_dependencies(&manifest, &self.router)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let catalogue = self.state.current_catalogue();
        let shelf =
            catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
                StewardError::Admission(format!(
                    "{}: target shelf not in catalogue: {}",
                    manifest.plugin.name, shelf_qualified
                ))
            })?;

        if !shelf.accepts_shape(manifest.target.shape) {
            let supports_note = if shelf.shape_supports.is_empty() {
                String::new()
            } else {
                format!(" (also accepts {:?})", shelf.shape_supports)
            };
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}{}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape,
                supports_note
            )));
        }

        if let Some(conflict) =
            preadmit_conflict_check(&self.router, &manifest, &shelf_qualified)
        {
            return Err(StewardError::Admission(format!(
                "{}: {conflict}",
                manifest.plugin.name
            )));
        }

        let manifest_kind = manifest.require_kind();
        if manifest_kind.interaction != InteractionShape::Warden {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_warden requires kind.interaction \
                 = 'warden', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.interaction
            )));
        }
        if manifest_kind.instance != InstanceShape::Singleton {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_warden requires kind.instance \
                 = 'singleton', manifest declares {:?} \
                 (use admit_factory_warden for factories)",
                manifest.plugin.name, manifest_kind.instance
            )));
        }

        let mut handle =
            AdmittedHandle::Warden(Box::new(WardenAdapter::new(plugin)));

        let description = handle.describe().await;
        if description.identity.name != manifest.plugin.name {
            return Err(StewardError::Admission(format!(
                "plugin describe() name {} does not match manifest name {}",
                description.identity.name, manifest.plugin.name
            )));
        }
        if description.identity.version != manifest.plugin.version {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() version {} does not match manifest version {}",
                manifest.plugin.name,
                description.identity.version,
                manifest.plugin.version
            )));
        }
        if description.identity.contract != manifest.plugin.contract {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() contract {} does not match manifest contract {}",
                manifest.plugin.name,
                description.identity.contract,
                manifest.plugin.contract
            )));
        }

        validation::check_drift_and_skew(
            &manifest,
            &description.runtime_capabilities,
            &self.state.bus,
        )
        .await?;

        let revoked =
            self.revoked_capabilities_for(&manifest.plugin.name).await?;
        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            self.state.current_catalogue(),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
            self.appointments_runtime.clone(),
            self.watches_runtime.clone(),
            self.stream_coordinator.clone(),
            self.metadata_chain.clone(),
            self.scheduler_runtime.clone(),
            self.ledger.clone(),
            self.audio_routing_runtime.clone(),
            self.audio_plane_runtime.clone(),
            self.group_store.clone(),
            self.multiroom_substrate.clone(),
            self.asset_cache.clone(),
            self.shelf_request_dispatcher.clone(),
            self.credential_vault.clone(),
            Arc::clone(&self.state.credential_change_bus),
            self.state.online_provider_config_store.get().cloned(),
            Arc::clone(&self.state.online_provider_config_bus),
            &revoked,
        )?;
        // Register URI schemes declared by the source plugin
        // before invoking load, so a scheme conflict refuses
        // admission before we pay the load cost. On load failure,
        // roll back the schemes to release them for retry.
        self.register_uri_schemes_for_manifest(&manifest).await?;
        let reprobe_task = match invoke_plugin_load(
            &mut handle,
            &mut ctx,
            &manifest.plugin.name,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.unregister_uri_schemes_for_manifest(&manifest).await;
                return Err(e);
            }
        };

        let kind_name = handle.kind_name();
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            trust_class = ?manifest.trust.class,
            "plugin admitted"
        );

        let entry = Arc::new(
            PluginEntry::new_with_policy(
                manifest.plugin.name.clone(),
                shelf_qualified.clone(),
                handle,
                EnforcementPolicy::from_manifest(&manifest),
            )
            .with_manifest(Arc::new(manifest.clone()))
            .with_stockings(manifest.stockings.clone())
            .with_reprobe(reprobe_task),
        );
        if let Err(e) = self.router.insert(entry) {
            // The duplicate-shelf check earlier in this method
            // covers the only expected failure mode; reaching
            // here implies a programming error or a late race.
            // Release the URI schemes we registered so the
            // operator can retry without manual cleanup.
            self.unregister_uri_schemes_for_manifest(&manifest).await;
            return Err(e);
        }

        // Record the plugin's version against its claimant token so
        // the `resolve_claimants` op can return both name and
        // version. Token derivation deliberately omits the version
        // (see [`crate::claimant`]), so the issuer needs an explicit
        // record_version call to populate the reverse-lookup row.
        self.state.claimant_issuer.record_version(
            &manifest.plugin.name,
            manifest.plugin.version.to_string(),
        );

        Ok(())
    }

    /// Admit an in-process plugin that exposes BOTH a warden
    /// surface and a respondent surface from the same plugin
    /// instance.
    ///
    /// Mirrors [`Self::admit_singleton_warden`] in every
    /// pre-admission check (manifest validation, prerequisites,
    /// trust, dependencies, shelf shape, kind = warden,
    /// instance = singleton). The single difference: the
    /// admitted handle is a
    /// [`AdmittedHandle::WardenWithRespondent`] wrapping a
    /// [`WardenAndRespondentAdapter<T>`] over the supplied
    /// plugin. The handle dispatches custody / course_correct
    /// through the warden surface and request_types through the
    /// respondent surface — both surfaces drive the same
    /// underlying plugin instance, so internal state stays
    /// consistent across the two dispatch planes.
    ///
    /// Canonical use case: an audio playback warden that owns
    /// one or more music URI schemes and answers source-verb
    /// dispatches (`play_now` / `play_now_collection`)
    /// targeting them. The warden holds playback custody;
    /// the respondent surface routes URI-targeted verbs to
    /// the same plugin's request handler.
    ///
    /// The plugin's manifest MUST declare both
    /// `[capabilities.warden]` and `[capabilities.respondent]`
    /// (the SDK manifest validator allows this asymmetric
    /// coexistence: warden + respondent is admitted, but
    /// respondent + warden is not).
    pub async fn admit_singleton_warden_with_respondent<T>(
        &mut self,
        plugin: T,
        manifest: Manifest,
    ) -> Result<(), StewardError>
    where
        T: Warden + Respondent + 'static,
    {
        manifest.validate()?;
        refuse_artefact_kind(
            &manifest,
            "admit_singleton_warden_with_respondent",
        )?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        self.check_and_record_ui_stockings(&manifest.plugin.name, &manifest)
            .await?;
        validation::check_dependencies(&manifest, &self.router)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let catalogue = self.state.current_catalogue();
        let shelf =
            catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
                StewardError::Admission(format!(
                    "{}: target shelf not in catalogue: {}",
                    manifest.plugin.name, shelf_qualified
                ))
            })?;

        if !shelf.accepts_shape(manifest.target.shape) {
            let supports_note = if shelf.shape_supports.is_empty() {
                String::new()
            } else {
                format!(" (also accepts {:?})", shelf.shape_supports)
            };
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}{}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape,
                supports_note
            )));
        }

        if let Some(conflict) =
            preadmit_conflict_check(&self.router, &manifest, &shelf_qualified)
        {
            return Err(StewardError::Admission(format!(
                "{}: {conflict}",
                manifest.plugin.name
            )));
        }

        let manifest_kind = manifest.require_kind();
        if manifest_kind.interaction != InteractionShape::Warden {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_warden_with_respondent requires \
                 kind.interaction = 'warden', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.interaction
            )));
        }
        if manifest_kind.instance != InstanceShape::Singleton {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_warden_with_respondent requires \
                 kind.instance = 'singleton', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.instance
            )));
        }
        // The defining shape of THIS admit method: the manifest
        // MUST declare a respondent block alongside the warden
        // block. A bare warden manifest belongs on the simpler
        // admit_singleton_warden path; the additional declaration
        // here unlocks the shared plugin-instance dispatch.
        if manifest.capabilities.respondent.is_none() {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_warden_with_respondent requires \
                 [capabilities.respondent] in the manifest; declare \
                 the respondent block (with at least one request_type) \
                 or use admit_singleton_warden for a pure warden",
                manifest.plugin.name
            )));
        }

        let mut handle = AdmittedHandle::WardenWithRespondent(Box::new(
            WardenAndRespondentAdapter::new(plugin),
        ));

        let description = handle.describe().await;
        if description.identity.name != manifest.plugin.name {
            return Err(StewardError::Admission(format!(
                "plugin describe() name {} does not match manifest name {}",
                description.identity.name, manifest.plugin.name
            )));
        }
        if description.identity.version != manifest.plugin.version {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() version {} does not match manifest version {}",
                manifest.plugin.name,
                description.identity.version,
                manifest.plugin.version
            )));
        }
        if description.identity.contract != manifest.plugin.contract {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() contract {} does not match manifest contract {}",
                manifest.plugin.name,
                description.identity.contract,
                manifest.plugin.contract
            )));
        }

        validation::check_drift_and_skew(
            &manifest,
            &description.runtime_capabilities,
            &self.state.bus,
        )
        .await?;

        let revoked =
            self.revoked_capabilities_for(&manifest.plugin.name).await?;
        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            self.state.current_catalogue(),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
            self.appointments_runtime.clone(),
            self.watches_runtime.clone(),
            self.stream_coordinator.clone(),
            self.metadata_chain.clone(),
            self.scheduler_runtime.clone(),
            self.ledger.clone(),
            self.audio_routing_runtime.clone(),
            self.audio_plane_runtime.clone(),
            self.group_store.clone(),
            self.multiroom_substrate.clone(),
            self.asset_cache.clone(),
            self.shelf_request_dispatcher.clone(),
            self.credential_vault.clone(),
            Arc::clone(&self.state.credential_change_bus),
            self.state.online_provider_config_store.get().cloned(),
            Arc::clone(&self.state.online_provider_config_bus),
            &revoked,
        )?;
        self.register_uri_schemes_for_manifest(&manifest).await?;
        let reprobe_task = match invoke_plugin_load(
            &mut handle,
            &mut ctx,
            &manifest.plugin.name,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.unregister_uri_schemes_for_manifest(&manifest).await;
                return Err(e);
            }
        };

        let kind_name = handle.kind_name();
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            trust_class = ?manifest.trust.class,
            "plugin admitted"
        );

        let entry = Arc::new(
            PluginEntry::new_with_policy(
                manifest.plugin.name.clone(),
                shelf_qualified.clone(),
                handle,
                EnforcementPolicy::from_manifest(&manifest),
            )
            .with_manifest(Arc::new(manifest.clone()))
            .with_stockings(manifest.stockings.clone())
            .with_reprobe(reprobe_task),
        );
        if let Err(e) = self.router.insert(entry) {
            self.unregister_uri_schemes_for_manifest(&manifest).await;
            return Err(e);
        }

        self.state.claimant_issuer.record_version(
            &manifest.plugin.name,
            manifest.plugin.version.to_string(),
        );

        Ok(())
    }

    /// Admit an in-process factory respondent.
    ///
    /// Parallel to [`Self::admit_singleton_respondent`] but for
    /// plugins whose `kind.instance = "factory"`. The plugin's
    /// [`RetractionPolicy`](evo_plugin_sdk::contract::factory::RetractionPolicy)
    /// is captured before the plugin is moved into the
    /// [`RespondentAdapter`] and used to construct a
    /// [`RegistryInstanceAnnouncer`] tagged with the plugin's name and
    /// target shelf. The announcer is placed in the plugin's
    /// [`LoadContext::instance_announcer`] slot, replacing the default
    /// [`LoggingInstanceAnnouncer`] used for singleton plugins. After
    /// `load` returns successfully, the announcer's `load_complete`
    /// flag flips so [`RetractionPolicy::StartupOnly`](evo_plugin_sdk::contract::factory::RetractionPolicy::StartupOnly)
    /// starts refusing further announces.
    ///
    /// Refuses non-factory manifests inline with a structured error
    /// pointing at [`Self::admit_singleton_respondent`] for the
    /// singleton path.
    pub async fn admit_factory_respondent<T>(
        &mut self,
        plugin: T,
        manifest: Manifest,
    ) -> Result<(), StewardError>
    where
        T: Factory + Respondent + 'static,
    {
        manifest.validate()?;
        refuse_artefact_kind(&manifest, "admit_factory_respondent")?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        self.check_and_record_ui_stockings(&manifest.plugin.name, &manifest)
            .await?;
        validation::check_dependencies(&manifest, &self.router)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let catalogue = self.state.current_catalogue();
        let shelf =
            catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
                StewardError::Admission(format!(
                    "{}: target shelf not in catalogue: {}",
                    manifest.plugin.name, shelf_qualified
                ))
            })?;

        if !shelf.accepts_shape(manifest.target.shape) {
            let supports_note = if shelf.shape_supports.is_empty() {
                String::new()
            } else {
                format!(" (also accepts {:?})", shelf.shape_supports)
            };
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}{}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape,
                supports_note
            )));
        }

        if let Some(conflict) =
            preadmit_conflict_check(&self.router, &manifest, &shelf_qualified)
        {
            return Err(StewardError::Admission(format!(
                "{}: {conflict}",
                manifest.plugin.name
            )));
        }

        let manifest_kind = manifest.require_kind();
        if manifest_kind.interaction != InteractionShape::Respondent {
            return Err(StewardError::Admission(format!(
                "{}: admit_factory_respondent requires kind.interaction \
                 = 'respondent', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.interaction
            )));
        }
        if manifest_kind.instance != InstanceShape::Factory {
            return Err(StewardError::Admission(format!(
                "{}: admit_factory_respondent requires kind.instance \
                 = 'factory', manifest declares {:?} \
                 (use admit_singleton_respondent for singletons)",
                manifest.plugin.name, manifest_kind.instance
            )));
        }

        // Capture the plugin's retraction policy before the move
        // into the adapter; the adapter sees only the Respondent
        // surface and the Factory trait would be unreachable
        // afterwards.
        let retraction_policy = plugin.retraction_policy();

        let mut handle = AdmittedHandle::Respondent(Box::new(
            RespondentAdapter::new(plugin),
        ));

        let description = handle.describe().await;
        if description.identity.name != manifest.plugin.name {
            return Err(StewardError::Admission(format!(
                "plugin describe() name {} does not match manifest name {}",
                description.identity.name, manifest.plugin.name
            )));
        }
        if description.identity.version != manifest.plugin.version {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() version {} does not match manifest version {}",
                manifest.plugin.name,
                description.identity.version,
                manifest.plugin.version
            )));
        }
        if description.identity.contract != manifest.plugin.contract {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() contract {} does not match manifest contract {}",
                manifest.plugin.name,
                description.identity.contract,
                manifest.plugin.contract
            )));
        }

        validation::check_drift_and_skew(
            &manifest,
            &description.runtime_capabilities,
            &self.state.bus,
        )
        .await?;

        let announcer = Arc::new(
            RegistryInstanceAnnouncer::new(
                Arc::clone(&self.state.subjects),
                Arc::clone(&self.state.bus),
                manifest.plugin.name.clone(),
                manifest.target.shelf.clone(),
                retraction_policy,
            )
            .with_persistence(Arc::clone(&self.state.persistence)),
        );

        let revoked =
            self.revoked_capabilities_for(&manifest.plugin.name).await?;
        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            self.state.current_catalogue(),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
            self.appointments_runtime.clone(),
            self.watches_runtime.clone(),
            self.stream_coordinator.clone(),
            self.metadata_chain.clone(),
            self.scheduler_runtime.clone(),
            self.ledger.clone(),
            self.audio_routing_runtime.clone(),
            self.audio_plane_runtime.clone(),
            self.group_store.clone(),
            self.multiroom_substrate.clone(),
            self.asset_cache.clone(),
            self.shelf_request_dispatcher.clone(),
            self.credential_vault.clone(),
            Arc::clone(&self.state.credential_change_bus),
            self.state.online_provider_config_store.get().cloned(),
            Arc::clone(&self.state.online_provider_config_bus),
            &revoked,
        )?;
        ctx.instance_announcer =
            Arc::clone(&announcer) as Arc<dyn InstanceAnnouncer>;

        // Register URI schemes declared by the source plugin
        // before invoking load, so a scheme conflict refuses
        // admission before we pay the load cost. On load failure,
        // roll back the schemes to release them for retry.
        self.register_uri_schemes_for_manifest(&manifest).await?;
        let reprobe_task = match invoke_plugin_load(
            &mut handle,
            &mut ctx,
            &manifest.plugin.name,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.unregister_uri_schemes_for_manifest(&manifest).await;
                return Err(e);
            }
        };

        // Flip the announcer's load_complete flag so any
        // RetractionPolicy::StartupOnly factory starts refusing
        // further announces from this point onward.
        announcer.mark_load_complete();
        self.factory_announcers
            .lock()
            .expect("factory announcers mutex poisoned")
            .insert(manifest.plugin.name.clone(), Arc::clone(&announcer));

        let kind_name = handle.kind_name();
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            instance_kind = "factory",
            retraction_policy = ?retraction_policy,
            trust_class = ?manifest.trust.class,
            "factory plugin admitted"
        );

        let entry = Arc::new(
            PluginEntry::new_with_policy(
                manifest.plugin.name.clone(),
                shelf_qualified.clone(),
                handle,
                EnforcementPolicy::from_manifest(&manifest),
            )
            .with_manifest(Arc::new(manifest.clone()))
            .with_stockings(manifest.stockings.clone())
            .with_reprobe(reprobe_task),
        );
        if let Err(e) = self.router.insert(entry) {
            // The duplicate-shelf check earlier in this method
            // covers the only expected failure mode; reaching
            // here implies a programming error or a late race.
            // Release the URI schemes we registered so the
            // operator can retry without manual cleanup.
            self.unregister_uri_schemes_for_manifest(&manifest).await;
            return Err(e);
        }

        self.state.claimant_issuer.record_version(
            &manifest.plugin.name,
            manifest.plugin.version.to_string(),
        );

        Ok(())
    }

    /// Admit an in-process factory warden.
    ///
    /// Parallel to [`Self::admit_singleton_warden`] but for plugins
    /// whose `kind.instance = "factory"`. See
    /// [`Self::admit_factory_respondent`] for the full discussion of
    /// the announcer wiring; the warden path differs only in that the
    /// plugin enters the router as an [`AdmittedHandle::Warden`].
    ///
    /// Custody handed to a factory warden's instance does not survive
    /// steward restart while custody / relation / admin durability
    /// remain in-memory only. Most natural-fit cases work anyway
    /// because the external entity (e.g. BlueZ pair record, network
    /// configuration, filesystem mount) carries the durable side of
    /// state; the plugin re-establishes evo's custody on its
    /// post-restart re-announce.
    pub async fn admit_factory_warden<T>(
        &mut self,
        plugin: T,
        manifest: Manifest,
    ) -> Result<(), StewardError>
    where
        T: Factory + Warden + 'static,
    {
        manifest.validate()?;
        refuse_artefact_kind(&manifest, "admit_factory_warden")?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        self.check_and_record_ui_stockings(&manifest.plugin.name, &manifest)
            .await?;
        validation::check_dependencies(&manifest, &self.router)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let catalogue = self.state.current_catalogue();
        let shelf =
            catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
                StewardError::Admission(format!(
                    "{}: target shelf not in catalogue: {}",
                    manifest.plugin.name, shelf_qualified
                ))
            })?;

        if !shelf.accepts_shape(manifest.target.shape) {
            let supports_note = if shelf.shape_supports.is_empty() {
                String::new()
            } else {
                format!(" (also accepts {:?})", shelf.shape_supports)
            };
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}{}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape,
                supports_note
            )));
        }

        if let Some(conflict) =
            preadmit_conflict_check(&self.router, &manifest, &shelf_qualified)
        {
            return Err(StewardError::Admission(format!(
                "{}: {conflict}",
                manifest.plugin.name
            )));
        }

        let manifest_kind = manifest.require_kind();
        if manifest_kind.interaction != InteractionShape::Warden {
            return Err(StewardError::Admission(format!(
                "{}: admit_factory_warden requires kind.interaction \
                 = 'warden', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.interaction
            )));
        }
        if manifest_kind.instance != InstanceShape::Factory {
            return Err(StewardError::Admission(format!(
                "{}: admit_factory_warden requires kind.instance \
                 = 'factory', manifest declares {:?} \
                 (use admit_singleton_warden for singletons)",
                manifest.plugin.name, manifest_kind.instance
            )));
        }

        let retraction_policy = plugin.retraction_policy();

        let mut handle =
            AdmittedHandle::Warden(Box::new(WardenAdapter::new(plugin)));

        let description = handle.describe().await;
        if description.identity.name != manifest.plugin.name {
            return Err(StewardError::Admission(format!(
                "plugin describe() name {} does not match manifest name {}",
                description.identity.name, manifest.plugin.name
            )));
        }
        if description.identity.version != manifest.plugin.version {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() version {} does not match manifest version {}",
                manifest.plugin.name,
                description.identity.version,
                manifest.plugin.version
            )));
        }
        if description.identity.contract != manifest.plugin.contract {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() contract {} does not match manifest contract {}",
                manifest.plugin.name,
                description.identity.contract,
                manifest.plugin.contract
            )));
        }

        validation::check_drift_and_skew(
            &manifest,
            &description.runtime_capabilities,
            &self.state.bus,
        )
        .await?;

        let announcer = Arc::new(
            RegistryInstanceAnnouncer::new(
                Arc::clone(&self.state.subjects),
                Arc::clone(&self.state.bus),
                manifest.plugin.name.clone(),
                manifest.target.shelf.clone(),
                retraction_policy,
            )
            .with_persistence(Arc::clone(&self.state.persistence)),
        );

        let revoked =
            self.revoked_capabilities_for(&manifest.plugin.name).await?;
        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            self.state.current_catalogue(),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
            self.appointments_runtime.clone(),
            self.watches_runtime.clone(),
            self.stream_coordinator.clone(),
            self.metadata_chain.clone(),
            self.scheduler_runtime.clone(),
            self.ledger.clone(),
            self.audio_routing_runtime.clone(),
            self.audio_plane_runtime.clone(),
            self.group_store.clone(),
            self.multiroom_substrate.clone(),
            self.asset_cache.clone(),
            self.shelf_request_dispatcher.clone(),
            self.credential_vault.clone(),
            Arc::clone(&self.state.credential_change_bus),
            self.state.online_provider_config_store.get().cloned(),
            Arc::clone(&self.state.online_provider_config_bus),
            &revoked,
        )?;
        ctx.instance_announcer =
            Arc::clone(&announcer) as Arc<dyn InstanceAnnouncer>;

        // Register URI schemes declared by the source plugin
        // before invoking load, so a scheme conflict refuses
        // admission before we pay the load cost. On load failure,
        // roll back the schemes to release them for retry.
        self.register_uri_schemes_for_manifest(&manifest).await?;
        let reprobe_task = match invoke_plugin_load(
            &mut handle,
            &mut ctx,
            &manifest.plugin.name,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.unregister_uri_schemes_for_manifest(&manifest).await;
                return Err(e);
            }
        };

        announcer.mark_load_complete();

        self.factory_announcers
            .lock()
            .expect("factory announcers mutex poisoned")
            .insert(manifest.plugin.name.clone(), Arc::clone(&announcer));

        let kind_name = handle.kind_name();
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            instance_kind = "factory",
            retraction_policy = ?retraction_policy,
            trust_class = ?manifest.trust.class,
            "factory plugin admitted"
        );

        let entry = Arc::new(
            PluginEntry::new_with_policy(
                manifest.plugin.name.clone(),
                shelf_qualified.clone(),
                handle,
                EnforcementPolicy::from_manifest(&manifest),
            )
            .with_manifest(Arc::new(manifest.clone()))
            .with_stockings(manifest.stockings.clone())
            .with_reprobe(reprobe_task),
        );
        if let Err(e) = self.router.insert(entry) {
            // The duplicate-shelf check earlier in this method
            // covers the only expected failure mode; reaching
            // here implies a programming error or a late race.
            // Release the URI schemes we registered so the
            // operator can retry without manual cleanup.
            self.unregister_uri_schemes_for_manifest(&manifest).await;
            return Err(e);
        }

        self.state.claimant_issuer.record_version(
            &manifest.plugin.name,
            manifest.plugin.version.to_string(),
        );

        Ok(())
    }

    /// Admit an out-of-process singleton respondent over the wire
    /// protocol.
    ///
    /// Takes the reader and writer halves of an already-established
    /// connection (Unix socket, TCP, test duplex) and a manifest.
    /// Follows the same admission sequence as
    /// [`Self::admit_singleton_respondent`]:
    ///
    /// 1. Validates manifest.
    /// 2. Verifies target shelf exists and has matching shape.
    /// 3. Verifies no plugin is already admitted on that shelf.
    /// 4. Spawns a [`WireRespondent`](crate::wire_client::WireRespondent)
    ///    against the reader/writer. The connect step performs the
    ///    `describe` handshake; identity is validated against the
    ///    manifest.
    /// 5. Constructs a `LoadContext` and calls `load` on the
    ///    respondent. The wire adapter forwards the load frame to the
    ///    remote plugin and installs event callbacks so asynchronous
    ///    events from the plugin reach the steward's registries.
    /// 6. Registers the plugin for request dispatch.
    ///
    /// On any failure the wire client is dropped (cleanly closing the
    /// connection) and an error is returned naming the specific reason.
    pub async fn admit_out_of_process_respondent<R, W>(
        &mut self,
        manifest: Manifest,
        reader: R,
        writer: W,
    ) -> Result<(), StewardError>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        manifest.validate()?;
        refuse_artefact_kind(&manifest, "admit_out_of_process_respondent")?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        self.check_and_record_ui_stockings(&manifest.plugin.name, &manifest)
            .await?;
        validation::check_dependencies(&manifest, &self.router)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let catalogue = self.state.current_catalogue();
        let shelf =
            catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
                StewardError::Admission(format!(
                    "{}: target shelf not in catalogue: {}",
                    manifest.plugin.name, shelf_qualified
                ))
            })?;

        if !shelf.accepts_shape(manifest.target.shape) {
            let supports_note = if shelf.shape_supports.is_empty() {
                String::new()
            } else {
                format!(" (also accepts {:?})", shelf.shape_supports)
            };
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}{}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape,
                supports_note
            )));
        }

        if let Some(conflict) =
            preadmit_conflict_check(&self.router, &manifest, &shelf_qualified)
        {
            return Err(StewardError::Admission(format!(
                "{}: {conflict}",
                manifest.plugin.name
            )));
        }

        let manifest_kind = manifest.require_kind();
        if manifest_kind.interaction != InteractionShape::Respondent {
            return Err(StewardError::Admission(format!(
                "{}: admit_out_of_process_respondent requires kind.interaction \
                 = 'respondent', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.interaction
            )));
        }

        // Connect and eagerly describe. If either the connection or
        // the initial describe fails we return here with no partial
        // state.
        let mut respondent = crate::wire_client::WireRespondent::connect(
            reader,
            writer,
            manifest.plugin.name.clone(),
        )
        .await
        .map_err(|e| {
            StewardError::Admission(format!(
                "{}: wire connect failed: {}",
                manifest.plugin.name, e
            ))
        })?;
        if let Some(ledger) = &self.prompt_ledger {
            respondent.set_prompt_ledger(Arc::clone(ledger));
        }

        let description = respondent.description();
        if description.identity.name != manifest.plugin.name {
            return Err(StewardError::Admission(format!(
                "plugin describe() name {} does not match manifest name {}",
                description.identity.name, manifest.plugin.name
            )));
        }
        if description.identity.version != manifest.plugin.version {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() version {} does not match manifest version {}",
                manifest.plugin.name,
                description.identity.version,
                manifest.plugin.version
            )));
        }
        if description.identity.contract != manifest.plugin.contract {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() contract {} does not match manifest contract {}",
                manifest.plugin.name,
                description.identity.contract,
                manifest.plugin.contract
            )));
        }

        validation::check_drift_and_skew(
            &manifest,
            &description.runtime_capabilities,
            &self.state.bus,
        )
        .await?;

        // Mint the audio-routing forwarder sink BEFORE wrapping
        // the respondent into the `AdmittedHandle`; the sink
        // clones the underlying WireClient's outbound sender +
        // cid counter + plugin name, so it stays valid after
        // the respondent moves into the handle. Used after
        // `invoke_plugin_load` succeeds to install the
        // framework forwarder that fans `publish_topology` hits
        // out as `WireFrame::AudioRoutingStateChanged` frames.
        let audio_routing_forwarder_sink =
            respondent.client().audio_routing_forwarder_sink();
        let multiroom_substrate_forwarder_sink =
            respondent.client().multiroom_substrate_forwarder_sink();
        let audio_plane_forwarder_sink =
            respondent.client().audio_plane_forwarder_sink();
        let credential_change_forwarder_sink =
            respondent.client().credential_change_forwarder_sink();
        let online_provider_config_forwarder_sink =
            respondent.client().online_provider_config_forwarder_sink();

        let mut handle = AdmittedHandle::Respondent(Box::new(respondent));

        let revoked =
            self.revoked_capabilities_for(&manifest.plugin.name).await?;
        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            self.state.current_catalogue(),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
            self.appointments_runtime.clone(),
            self.watches_runtime.clone(),
            self.stream_coordinator.clone(),
            self.metadata_chain.clone(),
            self.scheduler_runtime.clone(),
            self.ledger.clone(),
            self.audio_routing_runtime.clone(),
            self.audio_plane_runtime.clone(),
            self.group_store.clone(),
            self.multiroom_substrate.clone(),
            self.asset_cache.clone(),
            self.shelf_request_dispatcher.clone(),
            self.credential_vault.clone(),
            Arc::clone(&self.state.credential_change_bus),
            self.state.online_provider_config_store.get().cloned(),
            Arc::clone(&self.state.online_provider_config_bus),
            &revoked,
        )?;

        // For factory plugins, swap the default LoggingInstanceAnnouncer
        // for a real RegistryInstanceAnnouncer that mints subjects and
        // emits happenings. Out-of-process factories default to
        // RetractionPolicy::Dynamic; a future enhancement carries the
        // declared policy across the wire (or through a manifest field)
        // so StartupOnly / ShutdownOnly are operator-controllable for
        // OOP factories. The wiring layer's WireRespondent::load reads
        // ctx.instance_announcer when constructing the EventSink.
        let factory_announcer = if manifest_kind.instance
            == InstanceShape::Factory
        {
            let announcer = Arc::new(
                    RegistryInstanceAnnouncer::new(
                        Arc::clone(&self.state.subjects),
                        Arc::clone(&self.state.bus),
                        manifest.plugin.name.clone(),
                        manifest.target.shelf.clone(),
                        evo_plugin_sdk::contract::factory::RetractionPolicy::Dynamic,
                    )
                    .with_persistence(Arc::clone(&self.state.persistence)),
                );
            ctx.instance_announcer =
                Arc::clone(&announcer) as Arc<dyn InstanceAnnouncer>;
            Some(announcer)
        } else {
            None
        };

        // Register URI schemes declared by the source plugin
        // before invoking load, so a scheme conflict refuses
        // admission before we pay the load cost. On load failure,
        // roll back the schemes to release them for retry.
        self.register_uri_schemes_for_manifest(&manifest).await?;
        let reprobe_task = match invoke_plugin_load(
            &mut handle,
            &mut ctx,
            &manifest.plugin.name,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.unregister_uri_schemes_for_manifest(&manifest).await;
                return Err(e);
            }
        };

        // Install the audio-routing state-change forwarder
        // post-load. Wiring it before the plugin's `load`
        // completes would race against the SDK proxy's
        // construction inside the OOP host's `Load` frame
        // handler; the SDK dispatcher rejects
        // `AudioRoutingStateChanged` frames received before
        // `Load`. Skip when this plugin declares no audio
        // capability (ctx.audio_routing is None) or the engine
        // was constructed without an audio-routing runtime.
        if let (Some(runtime), Some(local_handle)) = (
            self.audio_routing_runtime.as_ref(),
            ctx.audio_routing.as_ref(),
        ) {
            crate::audio_routing::install_audio_routing_forwarder(
                Arc::clone(runtime),
                Arc::clone(local_handle),
                audio_routing_forwarder_sink,
                manifest.plugin.name.clone(),
            );
        }

        // Install the multi-room substrate forwarder. The
        // forwarder spawns two tasks subscribed to the local
        // substrate's role + group broadcast channels and
        // pushes every event as a wire frame onto the plugin's
        // outbound channel. Skips when this plugin's
        // LoadContext carries no substrate handle (test
        // harnesses, engines built without
        // `with_multiroom_substrate`). The spawned tasks
        // self-terminate when the substrate channels close or
        // the WireClient's outbound channel closes; the
        // JoinHandles are intentionally dropped here.
        if let Some(local_substrate) = ctx.multiroom_substrate.as_ref() {
            let _ = crate::wire_client::install_multiroom_substrate_forwarder(
                Arc::clone(local_substrate),
                multiroom_substrate_forwarder_sink,
            );
        }

        // Install the audio-plane forwarder. The forwarder
        // pushes the initial-state frame anchoring the SDK
        // proxy's `monotonic_ns` + `local_device_id` cache and
        // spawns three tokio tasks subscribed to the local
        // audio-plane handle's frame-received / frame-send-event
        // / frame-trace-report streams. Skips when this plugin
        // declares no audio-plane capability.
        if let Some(local_audio_plane) = ctx.audio_plane.as_ref() {
            let _ = crate::wire_client::install_audio_plane_forwarder(
                Arc::clone(local_audio_plane),
                audio_plane_forwarder_sink,
            )
            .await;
        }

        // Install the credential-change forwarder. Subscribes to
        // this plugin's own sender on the framework central
        // `CredentialChangeBus` and republishes every event as a
        // `WireFrame::CredentialSetChanged` frame on the plugin's
        // outbound wire channel. Idempotent — every OOP admission
        // installs one; the spawned task self-terminates when the
        // wire connection closes or the bus's sender drops.
        crate::wire_client::install_credential_change_forwarder(
            Arc::clone(&self.state.credential_change_bus),
            manifest.plugin.name.clone(),
            credential_change_forwarder_sink,
        );
        // Install the online-provider-config change forwarder.
        // Subscribes to the framework-global
        // `OnlineProviderConfigBus` and republishes every event
        // as a `WireFrame::OnlineProviderConfigChanged` frame on
        // the plugin's outbound wire channel. Plugin reactors
        // filter events by `provider_id` on receipt.
        crate::wire_client::install_online_provider_config_forwarder(
            Arc::clone(&self.state.online_provider_config_bus),
            online_provider_config_forwarder_sink,
        );

        if let Some(announcer) = &factory_announcer {
            announcer.mark_load_complete();
            self.factory_announcers
                .lock()
                .expect("factory announcers mutex poisoned")
                .insert(manifest.plugin.name.clone(), Arc::clone(announcer));
        }

        let kind_name = handle.kind_name();
        let instance_kind = if factory_announcer.is_some() {
            "factory"
        } else {
            "singleton"
        };
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            instance_kind,
            trust_class = ?manifest.trust.class,
            transport = "wire",
            "plugin admitted"
        );

        let entry = Arc::new(
            PluginEntry::new_with_policy(
                manifest.plugin.name.clone(),
                shelf_qualified.clone(),
                handle,
                EnforcementPolicy::from_manifest(&manifest),
            )
            .with_manifest(Arc::new(manifest.clone()))
            .with_stockings(manifest.stockings.clone())
            .with_reprobe(reprobe_task),
        );
        if let Err(e) = self.router.insert(entry) {
            // The duplicate-shelf check earlier in this method
            // covers the only expected failure mode; reaching
            // here implies a programming error or a late race.
            // Release the URI schemes we registered so the
            // operator can retry without manual cleanup.
            self.unregister_uri_schemes_for_manifest(&manifest).await;
            return Err(e);
        }

        // Record the plugin's version against its claimant token so
        // the `resolve_claimants` op can return both name and
        // version. Token derivation deliberately omits the version
        // (see [`crate::claimant`]), so the issuer needs an explicit
        // record_version call to populate the reverse-lookup row.
        self.state.claimant_issuer.record_version(
            &manifest.plugin.name,
            manifest.plugin.version.to_string(),
        );

        Ok(())
    }

    /// Admit an out-of-process warden over the wire protocol.
    ///
    /// Parallel to [`Self::admit_out_of_process_respondent`] for the
    /// warden interaction shape. Follows the same admission
    /// sequence: manifest validation, shelf lookup, shape match,
    /// duplicate check, interaction-kind check, wire connect +
    /// describe handshake, identity verification, load.
    ///
    /// On any failure the wire client is dropped (cleanly closing
    /// the connection) and an error is returned naming the specific
    /// reason.
    pub async fn admit_out_of_process_warden<R, W>(
        &mut self,
        manifest: Manifest,
        reader: R,
        writer: W,
    ) -> Result<(), StewardError>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        manifest.validate()?;
        refuse_artefact_kind(&manifest, "admit_out_of_process_warden")?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        self.check_and_record_ui_stockings(&manifest.plugin.name, &manifest)
            .await?;
        validation::check_dependencies(&manifest, &self.router)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let catalogue = self.state.current_catalogue();
        let shelf =
            catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
                StewardError::Admission(format!(
                    "{}: target shelf not in catalogue: {}",
                    manifest.plugin.name, shelf_qualified
                ))
            })?;

        if !shelf.accepts_shape(manifest.target.shape) {
            let supports_note = if shelf.shape_supports.is_empty() {
                String::new()
            } else {
                format!(" (also accepts {:?})", shelf.shape_supports)
            };
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}{}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape,
                supports_note
            )));
        }

        if let Some(conflict) =
            preadmit_conflict_check(&self.router, &manifest, &shelf_qualified)
        {
            return Err(StewardError::Admission(format!(
                "{}: {conflict}",
                manifest.plugin.name
            )));
        }

        let manifest_kind = manifest.require_kind();
        if manifest_kind.interaction != InteractionShape::Warden {
            return Err(StewardError::Admission(format!(
                "{}: admit_out_of_process_warden requires kind.interaction \
                 = 'warden', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.interaction
            )));
        }

        // Connect and eagerly describe. Hands the shared custody
        // ledger and happenings bus to WireWarden so its load()
        // installs a LedgerCustodyStateReporter in the event sink
        // that updates both.
        let mut warden = crate::wire_client::WireWarden::connect(
            reader,
            writer,
            manifest.plugin.name.clone(),
            Arc::clone(&self.state.custody),
            Arc::clone(&self.state.bus),
        )
        .await
        .map_err(|e| {
            StewardError::Admission(format!(
                "{}: wire connect failed: {}",
                manifest.plugin.name, e
            ))
        })?;
        if let Some(ledger) = &self.prompt_ledger {
            warden.set_prompt_ledger(Arc::clone(ledger));
        }

        let description = warden.description();
        if description.identity.name != manifest.plugin.name {
            return Err(StewardError::Admission(format!(
                "plugin describe() name {} does not match manifest name {}",
                description.identity.name, manifest.plugin.name
            )));
        }
        if description.identity.version != manifest.plugin.version {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() version {} does not match manifest version {}",
                manifest.plugin.name,
                description.identity.version,
                manifest.plugin.version
            )));
        }
        if description.identity.contract != manifest.plugin.contract {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() contract {} does not match manifest contract {}",
                manifest.plugin.name,
                description.identity.contract,
                manifest.plugin.contract
            )));
        }

        validation::check_drift_and_skew(
            &manifest,
            &description.runtime_capabilities,
            &self.state.bus,
        )
        .await?;

        // Mint the audio-routing forwarder sink BEFORE wrapping
        // the warden into the `AdmittedHandle`. See the parallel
        // block in `admit_out_of_process_respondent` for the
        // forwarder's purpose + post-load install rationale.
        let audio_routing_forwarder_sink =
            warden.client().audio_routing_forwarder_sink();
        let multiroom_substrate_forwarder_sink =
            warden.client().multiroom_substrate_forwarder_sink();
        let audio_plane_forwarder_sink =
            warden.client().audio_plane_forwarder_sink();
        let credential_change_forwarder_sink =
            warden.client().credential_change_forwarder_sink();
        let online_provider_config_forwarder_sink =
            warden.client().online_provider_config_forwarder_sink();

        let mut handle = AdmittedHandle::Warden(Box::new(warden));

        let revoked =
            self.revoked_capabilities_for(&manifest.plugin.name).await?;
        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            self.state.current_catalogue(),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
            self.appointments_runtime.clone(),
            self.watches_runtime.clone(),
            self.stream_coordinator.clone(),
            self.metadata_chain.clone(),
            self.scheduler_runtime.clone(),
            self.ledger.clone(),
            self.audio_routing_runtime.clone(),
            self.audio_plane_runtime.clone(),
            self.group_store.clone(),
            self.multiroom_substrate.clone(),
            self.asset_cache.clone(),
            self.shelf_request_dispatcher.clone(),
            self.credential_vault.clone(),
            Arc::clone(&self.state.credential_change_bus),
            self.state.online_provider_config_store.get().cloned(),
            Arc::clone(&self.state.online_provider_config_bus),
            &revoked,
        )?;

        // For factory plugins, swap the default LoggingInstanceAnnouncer
        // for a real RegistryInstanceAnnouncer. See the parallel block
        // in admit_out_of_process_respondent for the rationale +
        // RetractionPolicy::Dynamic default for OOP factories.
        let factory_announcer = if manifest_kind.instance
            == InstanceShape::Factory
        {
            let announcer = Arc::new(
                    RegistryInstanceAnnouncer::new(
                        Arc::clone(&self.state.subjects),
                        Arc::clone(&self.state.bus),
                        manifest.plugin.name.clone(),
                        manifest.target.shelf.clone(),
                        evo_plugin_sdk::contract::factory::RetractionPolicy::Dynamic,
                    )
                    .with_persistence(Arc::clone(&self.state.persistence)),
                );
            ctx.instance_announcer =
                Arc::clone(&announcer) as Arc<dyn InstanceAnnouncer>;
            Some(announcer)
        } else {
            None
        };

        // Register URI schemes declared by the source plugin
        // before invoking load, so a scheme conflict refuses
        // admission before we pay the load cost. On load failure,
        // roll back the schemes to release them for retry.
        self.register_uri_schemes_for_manifest(&manifest).await?;
        let reprobe_task = match invoke_plugin_load(
            &mut handle,
            &mut ctx,
            &manifest.plugin.name,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.unregister_uri_schemes_for_manifest(&manifest).await;
                return Err(e);
            }
        };

        // Install the audio-routing state-change forwarder
        // post-load. Same rationale as the respondent path.
        if let (Some(runtime), Some(local_handle)) = (
            self.audio_routing_runtime.as_ref(),
            ctx.audio_routing.as_ref(),
        ) {
            crate::audio_routing::install_audio_routing_forwarder(
                Arc::clone(runtime),
                Arc::clone(local_handle),
                audio_routing_forwarder_sink,
                manifest.plugin.name.clone(),
            );
        }

        // Install the multi-room substrate forwarder. The
        // forwarder spawns two tasks subscribed to the local
        // substrate's role + group broadcast channels and
        // pushes every event as a wire frame onto the plugin's
        // outbound channel. Skips when this plugin's
        // LoadContext carries no substrate handle (test
        // harnesses, engines built without
        // `with_multiroom_substrate`). The spawned tasks
        // self-terminate when the substrate channels close or
        // the WireClient's outbound channel closes; the
        // JoinHandles are intentionally dropped here.
        if let Some(local_substrate) = ctx.multiroom_substrate.as_ref() {
            let _ = crate::wire_client::install_multiroom_substrate_forwarder(
                Arc::clone(local_substrate),
                multiroom_substrate_forwarder_sink,
            );
        }

        // Install the audio-plane forwarder. The forwarder
        // pushes the initial-state frame anchoring the SDK
        // proxy's `monotonic_ns` + `local_device_id` cache and
        // spawns three tokio tasks subscribed to the local
        // audio-plane handle's frame-received / frame-send-event
        // / frame-trace-report streams. Skips when this plugin
        // declares no audio-plane capability.
        if let Some(local_audio_plane) = ctx.audio_plane.as_ref() {
            let _ = crate::wire_client::install_audio_plane_forwarder(
                Arc::clone(local_audio_plane),
                audio_plane_forwarder_sink,
            )
            .await;
        }

        // Install the credential-change forwarder. Subscribes to
        // this plugin's own sender on the framework central
        // `CredentialChangeBus` and republishes every event as a
        // `WireFrame::CredentialSetChanged` frame on the plugin's
        // outbound wire channel. Idempotent — every OOP admission
        // installs one; the spawned task self-terminates when the
        // wire connection closes or the bus's sender drops.
        crate::wire_client::install_credential_change_forwarder(
            Arc::clone(&self.state.credential_change_bus),
            manifest.plugin.name.clone(),
            credential_change_forwarder_sink,
        );
        // Install the online-provider-config change forwarder.
        // Subscribes to the framework-global
        // `OnlineProviderConfigBus` and republishes every event
        // as a `WireFrame::OnlineProviderConfigChanged` frame on
        // the plugin's outbound wire channel. Plugin reactors
        // filter events by `provider_id` on receipt.
        crate::wire_client::install_online_provider_config_forwarder(
            Arc::clone(&self.state.online_provider_config_bus),
            online_provider_config_forwarder_sink,
        );

        if let Some(announcer) = &factory_announcer {
            announcer.mark_load_complete();
            self.factory_announcers
                .lock()
                .expect("factory announcers mutex poisoned")
                .insert(manifest.plugin.name.clone(), Arc::clone(announcer));
        }

        let kind_name = handle.kind_name();
        let instance_kind = if factory_announcer.is_some() {
            "factory"
        } else {
            "singleton"
        };
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            instance_kind,
            trust_class = ?manifest.trust.class,
            transport = "wire",
            "plugin admitted"
        );

        let entry = Arc::new(
            PluginEntry::new_with_policy(
                manifest.plugin.name.clone(),
                shelf_qualified.clone(),
                handle,
                EnforcementPolicy::from_manifest(&manifest),
            )
            .with_manifest(Arc::new(manifest.clone()))
            .with_stockings(manifest.stockings.clone())
            .with_reprobe(reprobe_task),
        );
        if let Err(e) = self.router.insert(entry) {
            // The duplicate-shelf check earlier in this method
            // covers the only expected failure mode; reaching
            // here implies a programming error or a late race.
            // Release the URI schemes we registered so the
            // operator can retry without manual cleanup.
            self.unregister_uri_schemes_for_manifest(&manifest).await;
            return Err(e);
        }

        // Record the plugin's version against its claimant token so
        // the `resolve_claimants` op can return both name and
        // version. Token derivation deliberately omits the version
        // (see [`crate::claimant`]), so the issuer needs an explicit
        // record_version call to populate the reverse-lookup row.
        self.state.claimant_issuer.record_version(
            &manifest.plugin.name,
            manifest.plugin.version.to_string(),
        );

        Ok(())
    }

    /// Admit an out-of-process plugin that implements BOTH the
    /// warden + respondent contracts. Parallel to
    /// [`Self::admit_out_of_process_warden`] but registers as
    /// [`AdmittedHandle::WardenWithRespondent`]; routes
    /// course_correct / take_custody / release_custody through the
    /// warden surface AND routes handle_request through the
    /// respondent surface — both surfaces dispatch to the same OOP
    /// plugin instance over a single wire connection.
    ///
    /// The plugin's wire binary MUST use
    /// `evo_plugin_sdk::host::run_oop_warden_with_respondent` (or
    /// invoke `serve_combined` directly) so its dispatch loop
    /// accepts both frame classes.
    pub async fn admit_out_of_process_warden_with_respondent<R, W>(
        &mut self,
        manifest: Manifest,
        reader: R,
        writer: W,
    ) -> Result<(), StewardError>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        manifest.validate()?;
        refuse_artefact_kind(
            &manifest,
            "admit_out_of_process_warden_with_respondent",
        )?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        self.check_and_record_ui_stockings(&manifest.plugin.name, &manifest)
            .await?;
        validation::check_dependencies(&manifest, &self.router)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let catalogue = self.state.current_catalogue();
        let shelf =
            catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
                StewardError::Admission(format!(
                    "{}: target shelf not in catalogue: {}",
                    manifest.plugin.name, shelf_qualified
                ))
            })?;

        if !shelf.accepts_shape(manifest.target.shape) {
            let supports_note = if shelf.shape_supports.is_empty() {
                String::new()
            } else {
                format!(" (also accepts {:?})", shelf.shape_supports)
            };
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}{}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape,
                supports_note
            )));
        }

        if let Some(conflict) =
            preadmit_conflict_check(&self.router, &manifest, &shelf_qualified)
        {
            return Err(StewardError::Admission(format!(
                "{}: {conflict}",
                manifest.plugin.name
            )));
        }

        let manifest_kind = manifest.require_kind();
        if manifest_kind.interaction != InteractionShape::Warden {
            return Err(StewardError::Admission(format!(
                "{}: admit_out_of_process_warden_with_respondent requires \
                 kind.interaction = 'warden', manifest declares {:?}",
                manifest.plugin.name, manifest_kind.interaction
            )));
        }
        if manifest.capabilities.respondent.is_none() {
            return Err(StewardError::Admission(format!(
                "{}: admit_out_of_process_warden_with_respondent requires \
                 [capabilities.respondent] in the manifest; declare \
                 the respondent block (with at least one request_type) \
                 or use admit_out_of_process_warden for a pure warden",
                manifest.plugin.name
            )));
        }

        let mut adapter = crate::wire_client::WireWardenAndRespondent::connect(
            reader,
            writer,
            manifest.plugin.name.clone(),
            Arc::clone(&self.state.custody),
            Arc::clone(&self.state.bus),
        )
        .await
        .map_err(|e| {
            StewardError::Admission(format!(
                "{}: wire connect failed: {}",
                manifest.plugin.name, e
            ))
        })?;
        if let Some(ledger) = &self.prompt_ledger {
            adapter.set_prompt_ledger(Arc::clone(ledger));
        }

        let description = adapter.description().clone();
        if description.identity.name != manifest.plugin.name {
            return Err(StewardError::Admission(format!(
                "plugin describe() name {} does not match manifest name {}",
                description.identity.name, manifest.plugin.name
            )));
        }
        if description.identity.version != manifest.plugin.version {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() version {} does not match manifest version {}",
                manifest.plugin.name,
                description.identity.version,
                manifest.plugin.version
            )));
        }
        if description.identity.contract != manifest.plugin.contract {
            return Err(StewardError::Admission(format!(
                "{}: plugin describe() contract {} does not match manifest contract {}",
                manifest.plugin.name,
                description.identity.contract,
                manifest.plugin.contract
            )));
        }

        validation::check_drift_and_skew(
            &manifest,
            &description.runtime_capabilities,
            &self.state.bus,
        )
        .await?;

        // Mint the audio-routing forwarder sink BEFORE wrapping
        // the combined warden+respondent adapter into the
        // `AdmittedHandle`. See the parallel block in
        // `admit_out_of_process_respondent` for the forwarder's
        // purpose + post-load install rationale.
        let audio_routing_forwarder_sink =
            adapter.client().audio_routing_forwarder_sink();
        let multiroom_substrate_forwarder_sink =
            adapter.client().multiroom_substrate_forwarder_sink();
        let audio_plane_forwarder_sink =
            adapter.client().audio_plane_forwarder_sink();
        let credential_change_forwarder_sink =
            adapter.client().credential_change_forwarder_sink();
        let online_provider_config_forwarder_sink =
            adapter.client().online_provider_config_forwarder_sink();

        let mut handle =
            AdmittedHandle::WardenWithRespondent(Box::new(adapter));

        let revoked =
            self.revoked_capabilities_for(&manifest.plugin.name).await?;
        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            self.state.current_catalogue(),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
            self.appointments_runtime.clone(),
            self.watches_runtime.clone(),
            self.stream_coordinator.clone(),
            self.metadata_chain.clone(),
            self.scheduler_runtime.clone(),
            self.ledger.clone(),
            self.audio_routing_runtime.clone(),
            self.audio_plane_runtime.clone(),
            self.group_store.clone(),
            self.multiroom_substrate.clone(),
            self.asset_cache.clone(),
            self.shelf_request_dispatcher.clone(),
            self.credential_vault.clone(),
            Arc::clone(&self.state.credential_change_bus),
            self.state.online_provider_config_store.get().cloned(),
            Arc::clone(&self.state.online_provider_config_bus),
            &revoked,
        )?;
        let _ = &mut ctx; // ctx populated downstream by invoke_plugin_load

        self.register_uri_schemes_for_manifest(&manifest).await?;
        let reprobe_task = match invoke_plugin_load(
            &mut handle,
            &mut ctx,
            &manifest.plugin.name,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.unregister_uri_schemes_for_manifest(&manifest).await;
                return Err(e);
            }
        };

        // Install the audio-routing state-change forwarder
        // post-load. Same rationale as the respondent path.
        if let (Some(runtime), Some(local_handle)) = (
            self.audio_routing_runtime.as_ref(),
            ctx.audio_routing.as_ref(),
        ) {
            crate::audio_routing::install_audio_routing_forwarder(
                Arc::clone(runtime),
                Arc::clone(local_handle),
                audio_routing_forwarder_sink,
                manifest.plugin.name.clone(),
            );
        }

        // Install the multi-room substrate forwarder. The
        // forwarder spawns two tasks subscribed to the local
        // substrate's role + group broadcast channels and
        // pushes every event as a wire frame onto the plugin's
        // outbound channel. Skips when this plugin's
        // LoadContext carries no substrate handle (test
        // harnesses, engines built without
        // `with_multiroom_substrate`). The spawned tasks
        // self-terminate when the substrate channels close or
        // the WireClient's outbound channel closes; the
        // JoinHandles are intentionally dropped here.
        if let Some(local_substrate) = ctx.multiroom_substrate.as_ref() {
            let _ = crate::wire_client::install_multiroom_substrate_forwarder(
                Arc::clone(local_substrate),
                multiroom_substrate_forwarder_sink,
            );
        }

        // Install the audio-plane forwarder. The forwarder
        // pushes the initial-state frame anchoring the SDK
        // proxy's `monotonic_ns` + `local_device_id` cache and
        // spawns three tokio tasks subscribed to the local
        // audio-plane handle's frame-received / frame-send-event
        // / frame-trace-report streams. Skips when this plugin
        // declares no audio-plane capability.
        if let Some(local_audio_plane) = ctx.audio_plane.as_ref() {
            let _ = crate::wire_client::install_audio_plane_forwarder(
                Arc::clone(local_audio_plane),
                audio_plane_forwarder_sink,
            )
            .await;
        }

        // Install the credential-change forwarder. Subscribes to
        // this plugin's own sender on the framework central
        // `CredentialChangeBus` and republishes every event as a
        // `WireFrame::CredentialSetChanged` frame on the plugin's
        // outbound wire channel. Idempotent — every OOP admission
        // installs one; the spawned task self-terminates when the
        // wire connection closes or the bus's sender drops.
        crate::wire_client::install_credential_change_forwarder(
            Arc::clone(&self.state.credential_change_bus),
            manifest.plugin.name.clone(),
            credential_change_forwarder_sink,
        );
        // Install the online-provider-config change forwarder.
        // Subscribes to the framework-global
        // `OnlineProviderConfigBus` and republishes every event
        // as a `WireFrame::OnlineProviderConfigChanged` frame on
        // the plugin's outbound wire channel. Plugin reactors
        // filter events by `provider_id` on receipt.
        crate::wire_client::install_online_provider_config_forwarder(
            Arc::clone(&self.state.online_provider_config_bus),
            online_provider_config_forwarder_sink,
        );

        let kind_name = handle.kind_name();
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            instance_kind = "singleton",
            trust_class = ?manifest.trust.class,
            transport = "wire",
            "plugin admitted"
        );

        let entry = Arc::new(
            PluginEntry::new_with_policy(
                manifest.plugin.name.clone(),
                shelf_qualified.clone(),
                handle,
                EnforcementPolicy::from_manifest(&manifest),
            )
            .with_manifest(Arc::new(manifest.clone()))
            .with_stockings(manifest.stockings.clone())
            .with_reprobe(reprobe_task),
        );
        if let Err(e) = self.router.insert(entry) {
            self.unregister_uri_schemes_for_manifest(&manifest).await;
            return Err(e);
        }

        self.state.claimant_issuer.record_version(
            &manifest.plugin.name,
            manifest.plugin.version.to_string(),
        );

        Ok(())
    }

    /// Admit an out-of-process plugin from its on-disk bundle.
    ///
    /// Full soup-to-nuts admission from a plugin directory:
    ///
    /// 1. Reads `<plugin_dir>/manifest.toml` and validates it.
    /// 2. Requires `transport.kind == OutOfProcess`; returns an error
    ///    for in-process manifests (those must be registered via
    ///    [`Self::admit_singleton_respondent`] with a concrete Rust
    ///    type).
    /// 3. Resolves `transport.exec` relative to `plugin_dir`. Absolute
    ///    paths in `exec` are honoured as-is.
    /// 4. Computes a socket path under `runtime_dir` keyed by the
    ///    plugin name: `<runtime_dir>/<plugin-name>.sock`.
    /// 5. Spawns the plugin binary with that socket path as argv\[1\].
    ///    The child is spawned with `kill_on_drop(true)` so it cannot
    ///    outlive this engine even if shutdown is never called. On Unix,
    ///    if the engine was constructed with a non-default
    ///    [`PluginsSecurityConfig`] and
    ///    [`PluginsSecurityConfig::uid_gid_for_class`](crate::config::PluginsSecurityConfig::uid_gid_for_class)
    ///    returns an identity for the effective trust class, the child
    ///    is launched under that `setuid` / `setgid` identity; otherwise
    ///    it runs as the steward.
    /// 6. Polls for the socket to become connectable while checking
    ///    that the child has not already exited. Times out after
    ///    `SOCKET_READY_TIMEOUT` (5 s).
    /// 7. On a successful connection, splits the stream via
    ///    `into_split()` and hands the owned halves to either
    ///    [`Self::admit_out_of_process_respondent`] or
    ///    [`Self::admit_out_of_process_warden`], selected by the
    ///    manifest's `kind.interaction` field.
    /// 8. On success, retains the child handle on the admitted plugin
    ///    record so shutdown can wait on it.
    ///
    /// On any failure the child is killed and reaped before the error
    /// is returned; no partial state persists.
    ///
    /// ## Preconditions
    ///
    /// `runtime_dir` must exist and be writable by the steward user.
    /// The caller is responsible for creating it (systemd
    /// `RuntimeDirectory=evo`, a tempdir in tests, etc).
    pub async fn admit_out_of_process_from_directory(
        &mut self,
        plugin_dir: &Path,
        runtime_dir: &Path,
    ) -> Result<(), StewardError> {
        // Read and validate the manifest from disk.
        let manifest_path = plugin_dir.join("manifest.toml");
        let manifest_text =
            std::fs::read_to_string(&manifest_path).map_err(|e| {
                StewardError::io(
                    format!("reading {}", manifest_path.display()),
                    e,
                )
            })?;
        let mut manifest = Manifest::from_toml(&manifest_text)?;
        refuse_artefact_kind(&manifest, "admit_out_of_process_from_directory")?;
        validation::check_manifest_prerequisites(&manifest)?;

        // This entry point only handles out-of-process plugins. The
        // in-process path requires a concrete Rust type that cannot be
        // materialised from a manifest alone.
        let manifest_transport = manifest.require_transport();
        if manifest_transport.kind != TransportKind::OutOfProcess {
            return Err(StewardError::Admission(format!(
                "{}: admit_out_of_process_from_directory requires \
                 transport.type = 'out-of-process', manifest declares {:?}",
                manifest.plugin.name, manifest_transport.kind
            )));
        }

        // Resolve the executable path. Relative paths in `exec` are
        // relative to the plugin directory; absolute paths are used
        // verbatim. Absolute paths bypass the plugin-bundle convention
        // and are intended for test environments; production plugins
        // should always use relative paths.
        let exec_path = {
            let raw = Path::new(&manifest_transport.exec);
            if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                plugin_dir.join(raw)
            }
        };

        if let Some(t) = self.plugin_trust.as_ref() {
            use evo_trust::{
                verify_out_of_process_bundle, OutOfProcessBundleRef,
            };
            let bundle = OutOfProcessBundleRef {
                plugin_dir,
                manifest_path: &manifest_path,
                exec_path: &exec_path,
                plugin_name: &manifest.plugin.name,
                declared_trust: manifest.trust.class,
            };
            let o = verify_out_of_process_bundle(
                &bundle,
                &t.keys,
                &t.revocations,
                t.options,
            )
            .map_err(|e| StewardError::Admission(e.to_string()))?;
            manifest.trust.class = o.effective_trust;
            if o.was_unsigned {
                tracing::info!(
                    plugin = %manifest.plugin.name,
                    "admitting unsigned out-of-process plugin (sandbox) per allow_unsigned"
                );
            }
        }

        // Compulsory OS-dependency parity gate: read the plugin's
        // `privileges.yaml` (mandatory per the packaging contract)
        // and refuse admission when has_os_dependencies=true but
        // any declared required_binary is absent on the host. This
        // is the third and final wall — the distribution installer
        // (Strategy A) and `evo-plugin-tool install` (Strategy B)
        // already run the same check. Runs AFTER trust verify so
        // an unsigned or revoked bundle surfaces the trust-side
        // reason (more actionable) before the parity gate reads
        // the on-disk contract.
        enforce_privileges_parity_from_disk(plugin_dir, &manifest.plugin.name)?;

        // Compute the socket path. Dots in plugin names are valid in
        // filenames on all supported platforms.
        let socket_path =
            runtime_dir.join(format!("{}.sock", manifest.plugin.name));

        // Remove any stale socket from a previous crashed run. The
        // child will also try to remove it before binding, but doing
        // it here keeps the error surface clean if the child is not
        // the expected evo plugin binary.
        if socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&socket_path) {
                return Err(StewardError::io(
                    format!(
                        "removing stale socket at {}",
                        socket_path.display()
                    ),
                    e,
                ));
            }
        }

        // Spawn the child. Optional [plugins.security] applies per
        // effective trust class on Unix; non-Unix targets ignore the map.
        let mut cmd = Command::new(&exec_path);
        cmd.arg(&socket_path).kill_on_drop(true);
        #[cfg(unix)]
        {
            if let Some((uid, gid)) = self
                .plugins_security
                .uid_gid_for_class(manifest.trust.class)
            {
                // Inherent methods on `tokio::process::Command` (Unix)
                // mirror `std::os::unix::process::CommandExt`. Apply gid
                // before uid to match the usual drop-privilege order.
                cmd.gid(gid);
                cmd.uid(uid);
                tracing::info!(
                    plugin = %manifest.plugin.name,
                    class = ?manifest.trust.class,
                    uid,
                    gid,
                    "plugin process will run as mapped OS identity (plugins.security)"
                );
            }
        }
        let mut child = cmd.spawn().map_err(|e| {
            StewardError::io(
                format!("spawning plugin binary {}", exec_path.display()),
                e,
            )
        })?;

        tracing::info!(
            plugin = %manifest.plugin.name,
            exec = %exec_path.display(),
            socket = %socket_path.display(),
            pid = child.id().unwrap_or(0),
            "plugin process spawned"
        );

        // Wait for the socket to be ready, watching for early child
        // exit. On any failure, kill+reap the child before returning.
        let stream = match wait_for_socket_ready(&socket_path, &mut child).await
        {
            Ok(s) => s,
            Err(e) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(e);
            }
        };

        let (reader, writer) = stream.into_split();

        // Hand off to the appropriate wire-admission path based on
        // the manifest's interaction kind. On failure, kill+reap the
        // child.
        // Pick the admission path based on the manifest's
        // declared kind. A warden manifest that ALSO declares
        // `[capabilities.respondent]` is the combined shape:
        // both surfaces share one OOP plugin instance over one
        // wire connection (mirror of the in-process
        // `admit_singleton_warden_with_respondent` path).
        let manifest_kind = manifest.require_kind();
        let has_respondent_block = manifest.capabilities.respondent.is_some();
        let admission_result = match manifest_kind.interaction {
            InteractionShape::Respondent => {
                self.admit_out_of_process_respondent(
                    manifest.clone(),
                    reader,
                    writer,
                )
                .await
            }
            InteractionShape::Warden if has_respondent_block => {
                self.admit_out_of_process_warden_with_respondent(
                    manifest.clone(),
                    reader,
                    writer,
                )
                .await
            }
            InteractionShape::Warden => {
                self.admit_out_of_process_warden(
                    manifest.clone(),
                    reader,
                    writer,
                )
                .await
            }
        };
        if let Err(e) = admission_result {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(e);
        }

        // Admission succeeded. Attach the child to the admitted
        // record on the router so shutdown can reap it. Routes
        // by plugin name so multi-occupant respondent shelves
        // do not cross-attach a freshly admitted plugin's child
        // handle onto a sibling occupant's entry (with
        // `Command::kill_on_drop(true)` set on spawn, the
        // shelf-routed predecessor would otherwise SIGKILL the
        // first occupant's wire process when the second
        // co-occupant admitted).
        let plugin_name = manifest.plugin.name.clone();
        let shelf_qualified = manifest.target.shelf.clone();
        if !self.router.attach_child(&plugin_name, child).await {
            // Should be impossible: admission just inserted
            // this entry. If it is missing the child cannot be
            // reattached so the router has effectively lost
            // it. Log loudly. The child would have been moved
            // into attach_child if the entry existed, so
            // reaching here means the entry vanished before we
            // could attach.
            tracing::error!(
                plugin = %plugin_name,
                shelf = %shelf_qualified,
                "admitted record missing after successful admission"
            );
        }

        // Record the plugin's origin so the hot-reload path can
        // re-admit it from the same directory without operator
        // intervention.
        self.plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .insert(manifest.plugin.name.clone(), plugin_dir.to_path_buf());

        // Register the plugin in the gateway registry if its
        // manifest declares `[capabilities.gateway]`. Idempotent
        // on the plugin name; a re-admission overwrites the
        // prior entry with current values.
        self.register_gateway_if_declared(&manifest.plugin.name, &manifest)
            .await;

        Ok(())
    }

    /// Run signature verification against the configured
    /// `plugin_trust` for an artefact bundle and stamp the
    /// manifest's effective trust class on success.
    ///
    /// Mirror of the inline trust block in
    /// [`Self::admit_out_of_process_from_directory`] for the
    /// V2 artefact signing payload (manifest +
    /// content-tree digest). When `plugin_trust` is unset
    /// (test harness without trust roots), verification is
    /// skipped — the manifest's declared trust class is
    /// honoured verbatim, matching the functional path's
    /// posture under the same configuration.
    ///
    /// On failure, returns `StewardError::Admission` with
    /// the underlying trust error string so the operator
    /// sees the specific diagnostic (revoked, unsigned-
    /// inadmissible, signature-not-recognised, name-not-
    /// authorised, trust-class-not-authorised).
    fn verify_artefact_trust(
        &self,
        manifest: &mut Manifest,
        plugin_dir: &Path,
    ) -> Result<(), StewardError> {
        let Some(t) = self.plugin_trust.as_ref() else {
            return Ok(());
        };
        let manifest_path = plugin_dir.join("manifest.toml");
        let bundle = evo_trust::ArtefactBundleRef {
            plugin_dir,
            manifest_path: &manifest_path,
            plugin_name: &manifest.plugin.name,
            declared_trust: manifest.trust.class,
        };
        let outcome = evo_trust::verify_artefact_bundle(
            &bundle,
            &t.keys,
            &t.revocations,
            t.options,
        )
        .map_err(|e| StewardError::Admission(e.to_string()))?;
        manifest.trust.class = outcome.effective_trust;
        if outcome.was_unsigned {
            tracing::info!(
                plugin = %manifest.plugin.name,
                "admitting unsigned artefact (sandbox) per allow_unsigned"
            );
        }
        Ok(())
    }

    /// Admit a UI artefact bundle from a plugin directory on
    /// disk.
    ///
    /// Reads `manifest.toml` under `plugin_dir`, validates +
    /// dispatches the parsed manifest to the matching
    /// per-kind path (theme / ui_shell / widget_kind_pack),
    /// and refuses functional manifests at this entry —
    /// functional plugins flow through
    /// [`Self::admit_out_of_process_from_directory`] instead.
    ///
    /// Discovery dispatches artefact-kind manifests here once
    /// they're recognised at parse time; operators invoking
    /// the install verb directly (sub-primitive C) reach the
    /// same gauntlet.
    pub async fn admit_artefact_from_directory(
        &mut self,
        plugin_dir: &Path,
    ) -> Result<(), StewardError> {
        let manifest_path = plugin_dir.join("manifest.toml");
        let manifest_text =
            std::fs::read_to_string(&manifest_path).map_err(|e| {
                StewardError::io(
                    format!("reading {}", manifest_path.display()),
                    e,
                )
            })?;
        let manifest = Manifest::from_toml(&manifest_text)?;

        match manifest.plugin.kind {
            ArtefactKind::Functional => Err(StewardError::Admission(format!(
                "{}: admit_artefact_from_directory rejects functional \
                     plugins; use admit_out_of_process_from_directory",
                manifest.plugin.name,
            ))),
            ArtefactKind::Theme => self.admit_theme(manifest, plugin_dir).await,
            ArtefactKind::UiShell => {
                self.admit_ui_shell(manifest, plugin_dir).await
            }
            ArtefactKind::WidgetKindPack => {
                self.admit_widget_kind_pack(manifest, plugin_dir).await
            }
        }
    }

    /// Admit a theme artefact: validate the manifest,
    /// register the theme bundle into [`ThemeRegistry`].
    ///
    /// Theme assets (logos, fonts, sounds) are not validated
    /// for filesystem presence at admission; the renderer
    /// surfaces missing-asset diagnostics at render time when
    /// it tries to resolve a token's asset reference. The
    /// admission gate's job is metadata: that the theme
    /// declares the right shape and registers without
    /// collision.
    ///
    /// [`ThemeRegistry`]: crate::ui_registry::ThemeRegistry
    pub async fn admit_theme(
        &mut self,
        mut manifest: Manifest,
        plugin_dir: &Path,
    ) -> Result<(), StewardError> {
        manifest.validate()?;
        if manifest.plugin.kind != ArtefactKind::Theme {
            return Err(StewardError::Admission(format!(
                "{}: admit_theme requires plugin.kind = \"theme\", \
                 manifest declares {:?}",
                manifest.plugin.name, manifest.plugin.kind
            )));
        }
        self.verify_artefact_trust(&mut manifest, plugin_dir)?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        let registry = self.ui_themes.as_ref().ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: admit_theme requires the theme registry; \
                 wire one via AdmissionEngine::with_ui_themes",
                manifest.plugin.name,
            ))
        })?;

        let theme_section = manifest.theme.clone().ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: theme manifest missing [theme] section despite \
                 validate() passing — invariant violation",
                manifest.plugin.name,
            ))
        })?;

        let admitted = crate::ui_registry::AdmittedTheme {
            plugin_name: manifest.plugin.name.clone(),
            plugin_version: manifest.plugin.version.clone(),
            plugin_dir: plugin_dir.to_path_buf(),
            section: theme_section,
        };

        registry.register(admitted).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: theme registry refused: {e}",
                manifest.plugin.name,
            ))
        })?;
        // Record plugin_origin so the bundled-roots policy
        // (consulted by `uninstall_plugin`) applies to this
        // artefact identically to functional plugins.
        self.plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .insert(manifest.plugin.name.clone(), plugin_dir.to_path_buf());
        let _ = self
            .state
            .bus
            .emit_durable(crate::happenings::Happening::UiThemeAdmitted {
                plugin: manifest.plugin.name.clone(),
                version: manifest.plugin.version.to_string(),
                at: std::time::SystemTime::now(),
            })
            .await;
        tracing::info!(
            plugin = %manifest.plugin.name,
            version = %manifest.plugin.version,
            plugin_dir = %plugin_dir.display(),
            "admitted theme artefact"
        );
        Ok(())
    }

    /// Admit a UI shell artefact: validate the manifest,
    /// register the shell bundle into [`UiShellRegistry`].
    ///
    /// The shell's entry-point file is not opened at
    /// admission; the renderer loads it lazily on
    /// activation. Admission validates the manifest's
    /// metadata is well-shaped and the shell registers
    /// without collision.
    ///
    /// [`UiShellRegistry`]: crate::ui_registry::UiShellRegistry
    pub async fn admit_ui_shell(
        &mut self,
        mut manifest: Manifest,
        plugin_dir: &Path,
    ) -> Result<(), StewardError> {
        manifest.validate()?;
        if manifest.plugin.kind != ArtefactKind::UiShell {
            return Err(StewardError::Admission(format!(
                "{}: admit_ui_shell requires plugin.kind = \"ui_shell\", \
                 manifest declares {:?}",
                manifest.plugin.name, manifest.plugin.kind
            )));
        }
        self.verify_artefact_trust(&mut manifest, plugin_dir)?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;
        let registry = self.ui_shells.as_ref().ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: admit_ui_shell requires the UI shell registry; \
                 wire one via AdmissionEngine::with_ui_shells",
                manifest.plugin.name,
            ))
        })?;

        let shell_section = manifest.ui_shell.clone().ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: ui_shell manifest missing [ui_shell] section despite \
                 validate() passing — invariant violation",
                manifest.plugin.name,
            ))
        })?;

        let admitted = crate::ui_registry::AdmittedUiShell {
            plugin_name: manifest.plugin.name.clone(),
            plugin_version: manifest.plugin.version.clone(),
            plugin_dir: plugin_dir.to_path_buf(),
            section: shell_section,
        };

        registry.register(admitted).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: ui shell registry refused: {e}",
                manifest.plugin.name,
            ))
        })?;
        self.plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .insert(manifest.plugin.name.clone(), plugin_dir.to_path_buf());
        let _ = self
            .state
            .bus
            .emit_durable(crate::happenings::Happening::UiShellAdmitted {
                plugin: manifest.plugin.name.clone(),
                version: manifest.plugin.version.to_string(),
                at: std::time::SystemTime::now(),
            })
            .await;
        tracing::info!(
            plugin = %manifest.plugin.name,
            version = %manifest.plugin.version,
            plugin_dir = %plugin_dir.display(),
            "admitted ui_shell artefact"
        );
        Ok(())
    }

    /// Admit a widget kind pack artefact: validate the
    /// manifest, read + parse the size-envelopes and
    /// accessibility-declarations side files, fold the kinds
    /// into the framework's [`WidgetKindRegistry`], record
    /// the pack's metadata + a11y declarations into
    /// [`WidgetKindPackRegistry`].
    ///
    /// The three sets of kind ids — `[widgets].provides`,
    /// the size-envelopes file's keys, the
    /// accessibility-declarations file's keys — MUST be
    /// equal; admission refuses on any mismatch with a
    /// diagnostic naming the offending side. Each kind id
    /// MUST be absent from the widget-kind registry already;
    /// admission refuses on collision with another pack.
    /// Failure rolls back any partial registration so the
    /// pack is all-or-nothing.
    ///
    /// [`WidgetKindRegistry`]: crate::ui_registry::WidgetKindRegistry
    /// [`WidgetKindPackRegistry`]: crate::ui_registry::WidgetKindPackRegistry
    pub async fn admit_widget_kind_pack(
        &mut self,
        mut manifest: Manifest,
        plugin_dir: &Path,
    ) -> Result<(), StewardError> {
        manifest.validate()?;
        if manifest.plugin.kind != ArtefactKind::WidgetKindPack {
            return Err(StewardError::Admission(format!(
                "{}: admit_widget_kind_pack requires plugin.kind = \
                 \"widget_kind_pack\", manifest declares {:?}",
                manifest.plugin.name, manifest.plugin.kind
            )));
        }
        self.verify_artefact_trust(&mut manifest, plugin_dir)?;
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;

        let pack_registry = self.ui_widget_packs.as_ref().ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: admit_widget_kind_pack requires the widget-kind \
                     pack registry; wire one via \
                     AdmissionEngine::with_ui_widget_packs",
                manifest.plugin.name,
            ))
        })?;
        let widget_registry = self.ui_widgets.as_ref().ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: admit_widget_kind_pack requires the widget-kind \
                 registry; wire one via AdmissionEngine::with_ui_widgets",
                manifest.plugin.name,
            ))
        })?;

        let widgets_section = manifest.widgets.clone().ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: widget_kind_pack manifest missing [widgets] section \
                 despite validate() passing — invariant violation",
                manifest.plugin.name,
            ))
        })?;

        let envelopes_path =
            plugin_dir.join(&widgets_section.size_envelopes_path);
        let envelopes_text =
            std::fs::read_to_string(&envelopes_path).map_err(|e| {
                StewardError::io(
                    format!(
                        "reading size envelopes file {}",
                        envelopes_path.display()
                    ),
                    e,
                )
            })?;
        let envelopes_file =
            evo_plugin_sdk::widget_pack::WidgetSizeEnvelopesFile::from_toml(
                &envelopes_text,
            )
            .map_err(|e| {
                StewardError::Admission(format!(
                    "{}: invalid size envelopes file {}: {e}",
                    manifest.plugin.name,
                    envelopes_path.display(),
                ))
            })?;

        let a11y_path =
            plugin_dir.join(&widgets_section.accessibility_declarations_path);
        let a11y_text = std::fs::read_to_string(&a11y_path).map_err(|e| {
            StewardError::io(
                format!(
                    "reading accessibility declarations file {}",
                    a11y_path.display()
                ),
                e,
            )
        })?;
        let a11y_file = evo_plugin_sdk::widget_pack::WidgetAccessibilityDeclarationsFile::from_toml(
            &a11y_text,
        ).map_err(|e| {
            StewardError::Admission(format!(
                "{}: invalid accessibility declarations file {}: {e}",
                manifest.plugin.name,
                a11y_path.display(),
            ))
        })?;

        // Cross-file set equality on kind ids. The pack is
        // all-or-nothing; partial side files are an authoring
        // error.
        let manifest_set: std::collections::BTreeSet<&str> = widgets_section
            .provides
            .iter()
            .map(String::as_str)
            .collect();
        let envelopes_set: std::collections::BTreeSet<&str> = envelopes_file
            .envelopes
            .keys()
            .map(String::as_str)
            .collect();
        let a11y_set: std::collections::BTreeSet<&str> =
            a11y_file.declarations.keys().map(String::as_str).collect();
        if manifest_set != envelopes_set {
            return Err(StewardError::Admission(format!(
                "{}: kind ids declared in [widgets].provides \
                 ({:?}) do not match keys in size envelopes file \
                 ({:?})",
                manifest.plugin.name, manifest_set, envelopes_set,
            )));
        }
        if manifest_set != a11y_set {
            return Err(StewardError::Admission(format!(
                "{}: kind ids declared in [widgets].provides \
                 ({:?}) do not match keys in accessibility \
                 declarations file ({:?})",
                manifest.plugin.name, manifest_set, a11y_set,
            )));
        }

        // Collision check before registering: every kind id
        // in the pack must be absent from the widget-kind
        // registry.
        for kind_id in &widgets_section.provides {
            if widget_registry.contains(kind_id).await {
                return Err(StewardError::Admission(format!(
                    "{}: widget kind {:?} already registered \
                     (likely by a previously-admitted pack)",
                    manifest.plugin.name, kind_id,
                )));
            }
        }

        // Register the kinds first. Roll back on any failure
        // so the pack is all-or-nothing.
        let mut registered: Vec<String> = Vec::new();
        for (kind_id, envelope) in &envelopes_file.envelopes {
            if let Err(e) = widget_registry.register(envelope.clone()).await {
                for already in &registered {
                    let _ = widget_registry.unregister(already).await;
                }
                return Err(StewardError::Admission(format!(
                    "{}: widget-kind registry refused {kind_id}: {e}",
                    manifest.plugin.name,
                )));
            }
            registered.push(kind_id.clone());
        }

        let admitted = crate::ui_registry::AdmittedWidgetKindPack {
            plugin_name: manifest.plugin.name.clone(),
            plugin_version: manifest.plugin.version.clone(),
            plugin_dir: plugin_dir.to_path_buf(),
            section: widgets_section,
            accessibility: a11y_file.declarations,
        };

        if let Err(e) = pack_registry.register(admitted).await {
            // Roll back the kinds registered above before
            // surfacing the diagnostic.
            for already in &registered {
                let _ = widget_registry.unregister(already).await;
            }
            return Err(StewardError::Admission(format!(
                "{}: widget-kind-pack registry refused: {e}",
                manifest.plugin.name,
            )));
        }
        self.plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .insert(manifest.plugin.name.clone(), plugin_dir.to_path_buf());
        let _ = self
            .state
            .bus
            .emit_durable(crate::happenings::Happening::UiWidgetPackAdmitted {
                plugin: manifest.plugin.name.clone(),
                version: manifest.plugin.version.to_string(),
                kind_count: registered.len() as u32,
                at: std::time::SystemTime::now(),
            })
            .await;
        tracing::info!(
            plugin = %manifest.plugin.name,
            version = %manifest.plugin.version,
            plugin_dir = %plugin_dir.display(),
            kind_count = registered.len(),
            "admitted widget_kind_pack artefact"
        );
        Ok(())
    }

    /// Classify an admitted plugin name as one of the
    /// artefact variants by checking each registry. Returns
    /// `None` when the name is not in any artefact registry
    /// (which is also the answer for a functional plugin
    /// whose lookup goes through the router instead).
    ///
    /// Used by [`Self::uninstall_plugin`] to dispatch
    /// removal: artefacts flow through the unadmit paths
    /// below; functional plugins flow through the existing
    /// router-driven uninstall machinery.
    pub async fn lookup_artefact_kind(
        &self,
        plugin_name: &str,
    ) -> Option<ArtefactKind> {
        if let Some(registry) = self.ui_themes.as_ref() {
            if registry.contains(plugin_name).await {
                return Some(ArtefactKind::Theme);
            }
        }
        if let Some(registry) = self.ui_shells.as_ref() {
            if registry.contains(plugin_name).await {
                return Some(ArtefactKind::UiShell);
            }
        }
        if let Some(registry) = self.ui_widget_packs.as_ref() {
            if registry.contains(plugin_name).await {
                return Some(ArtefactKind::WidgetKindPack);
            }
        }
        None
    }

    /// Unadmit an artefact by canonical plugin name.
    ///
    /// Dispatches by artefact kind: looks up which registry
    /// holds the plugin, calls the matching per-kind unadmit
    /// path. Refuses with `Admission(... not admitted)`
    /// when the name is not in any artefact registry.
    pub async fn unadmit_artefact(
        &self,
        plugin_name: &str,
    ) -> Result<ArtefactKind, StewardError> {
        match self.lookup_artefact_kind(plugin_name).await {
            Some(ArtefactKind::Theme) => {
                self.unadmit_theme(plugin_name).await?;
                Ok(ArtefactKind::Theme)
            }
            Some(ArtefactKind::UiShell) => {
                self.unadmit_ui_shell(plugin_name).await?;
                Ok(ArtefactKind::UiShell)
            }
            Some(ArtefactKind::WidgetKindPack) => {
                self.unadmit_widget_kind_pack(plugin_name).await?;
                Ok(ArtefactKind::WidgetKindPack)
            }
            Some(ArtefactKind::Functional) | None => {
                Err(StewardError::Admission(format!(
                    "unadmit_artefact: plugin {plugin_name:?} is not \
                     admitted in any artefact registry"
                )))
            }
        }
    }

    /// Unadmit a theme: clear the active-theme slot if this
    /// plugin is currently active (auto-clear discipline),
    /// then unregister from [`ThemeRegistry`], drop the
    /// recorded plugin origin, and emit
    /// [`Happening::UiThemeUnadmitted`].
    ///
    /// Refuses with `Admission(... not admitted)` when the
    /// plugin is not in the theme registry.
    ///
    /// [`ThemeRegistry`]: crate::ui_registry::ThemeRegistry
    /// [`Happening::UiThemeUnadmitted`]: crate::happenings::Happening::UiThemeUnadmitted
    pub async fn unadmit_theme(
        &self,
        plugin_name: &str,
    ) -> Result<(), StewardError> {
        let registry = self.ui_themes.as_ref().ok_or_else(|| {
            StewardError::Admission(format!(
                "{plugin_name}: unadmit_theme requires the theme \
                 registry; wire one via with_ui_themes"
            ))
        })?;

        // Auto-clear the active theme if it's this plugin.
        // Done BEFORE the unregister so the
        // UiActiveThemeChanged event fires while the
        // active-selection runtime can still look the
        // plugin up in the registry (registry contents
        // don't matter for clear, but ordering matters for
        // subscriber sequencing — clear first, then
        // available-set drop).
        if let Some(active) = self.ui_active.as_ref() {
            if active.active_theme().await.as_deref() == Some(plugin_name) {
                if let Err(e) =
                    active.set_active_theme(None, "system:unadmit").await
                {
                    tracing::warn!(
                        plugin = plugin_name,
                        error = %e,
                        "unadmit_theme: auto-clear failed; proceeding \
                         with unadmit anyway"
                    );
                }
            }
        }

        registry.unregister(plugin_name).await.map_err(|e| {
            StewardError::Admission(format!(
                "{plugin_name}: theme registry refused unregister: {e}"
            ))
        })?;
        self.plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .remove(plugin_name);
        let _ = self
            .state
            .bus
            .emit_durable(crate::happenings::Happening::UiThemeUnadmitted {
                plugin: plugin_name.to_string(),
                at: std::time::SystemTime::now(),
            })
            .await;
        tracing::info!(plugin = plugin_name, "unadmitted theme artefact");
        Ok(())
    }

    /// Unadmit a UI shell: mirror of [`Self::unadmit_theme`]
    /// for the `ui_shell` slot.
    pub async fn unadmit_ui_shell(
        &self,
        plugin_name: &str,
    ) -> Result<(), StewardError> {
        let registry = self.ui_shells.as_ref().ok_or_else(|| {
            StewardError::Admission(format!(
                "{plugin_name}: unadmit_ui_shell requires the UI shell \
                 registry; wire one via with_ui_shells"
            ))
        })?;

        if let Some(active) = self.ui_active.as_ref() {
            if active.active_ui_shell().await.as_deref() == Some(plugin_name) {
                if let Err(e) =
                    active.set_active_ui_shell(None, "system:unadmit").await
                {
                    tracing::warn!(
                        plugin = plugin_name,
                        error = %e,
                        "unadmit_ui_shell: auto-clear failed; \
                         proceeding with unadmit anyway"
                    );
                }
            }
        }

        registry.unregister(plugin_name).await.map_err(|e| {
            StewardError::Admission(format!(
                "{plugin_name}: ui shell registry refused unregister: {e}"
            ))
        })?;
        self.plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .remove(plugin_name);
        let _ = self
            .state
            .bus
            .emit_durable(crate::happenings::Happening::UiShellUnadmitted {
                plugin: plugin_name.to_string(),
                at: std::time::SystemTime::now(),
            })
            .await;
        tracing::info!(plugin = plugin_name, "unadmitted ui_shell artefact");
        Ok(())
    }

    /// Unadmit a widget kind pack: roll back every kind the
    /// pack contributed to [`WidgetKindRegistry`], drop the
    /// pack from [`WidgetKindPackRegistry`], drop the
    /// recorded plugin origin, and emit
    /// [`Happening::UiWidgetPackUnadmitted`] with the
    /// rolled-back kind count so subscribers can invalidate
    /// matching widget resolution caches.
    ///
    /// Widget kind packs have no active-selection slot —
    /// every admitted pack contributes its kinds to the
    /// union; unadmission removes them. No auto-clear path
    /// to take (the active-selection registries don't carry
    /// a widget-pack slot).
    ///
    /// [`WidgetKindRegistry`]: crate::ui_registry::WidgetKindRegistry
    /// [`WidgetKindPackRegistry`]: crate::ui_registry::WidgetKindPackRegistry
    /// [`Happening::UiWidgetPackUnadmitted`]: crate::happenings::Happening::UiWidgetPackUnadmitted
    pub async fn unadmit_widget_kind_pack(
        &self,
        plugin_name: &str,
    ) -> Result<(), StewardError> {
        let pack_registry = self.ui_widget_packs.as_ref().ok_or_else(|| {
            StewardError::Admission(format!(
                "{plugin_name}: unadmit_widget_kind_pack requires \
                     the widget-kind-pack registry"
            ))
        })?;
        let widget_registry = self.ui_widgets.as_ref().ok_or_else(|| {
            StewardError::Admission(format!(
                "{plugin_name}: unadmit_widget_kind_pack requires \
                 the widget-kind registry"
            ))
        })?;

        let pack =
            pack_registry.unregister(plugin_name).await.map_err(|e| {
                StewardError::Admission(format!(
                    "{plugin_name}: widget-kind-pack registry refused \
                 unregister: {e}"
                ))
            })?;

        // Roll back every kind the pack contributed. Best
        // effort: a missing kind (already gone for some
        // reason) doesn't block the unadmission — log and
        // continue.
        let mut rolled_back = 0u32;
        for kind_id in &pack.section.provides {
            match widget_registry.unregister(kind_id).await {
                Ok(_) => rolled_back += 1,
                Err(e) => {
                    tracing::warn!(
                        plugin = plugin_name,
                        kind = kind_id,
                        error = %e,
                        "unadmit_widget_kind_pack: kind rollback failed; \
                         continuing"
                    );
                }
            }
        }

        self.plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .remove(plugin_name);
        let _ = self
            .state
            .bus
            .emit_durable(
                crate::happenings::Happening::UiWidgetPackUnadmitted {
                    plugin: plugin_name.to_string(),
                    kind_count: rolled_back,
                    at: std::time::SystemTime::now(),
                },
            )
            .await;
        tracing::info!(
            plugin = plugin_name,
            kind_count = rolled_back,
            "unadmitted widget_kind_pack artefact"
        );
        Ok(())
    }

    /// Hot-reload a single admitted plugin per its declared
    /// `lifecycle.mode`.
    ///
    /// Dispatch on the mode the plugin's manifest declares:
    ///
    /// - `LifecycleMode::Frozen` (default for manifests that
    ///   omit the field): refused with a structured error.
    ///   Restart the steward to apply TOML changes.
    /// - `LifecycleMode::ReactiveOnly`: acknowledged without
    ///   teardown. The plugin's operator state lives in
    ///   framework substrates and is updated via wire ops;
    ///   the plugin's existing substrate subscriptions are
    ///   the reactivity mechanism, not a teardown + re-admit
    ///   cycle.
    /// - `LifecycleMode::ReloadCleanable`: full teardown +
    ///   re-admit cycle. Reachable only for plugins admitted
    ///   via [`Self::admit_out_of_process_from_directory`] —
    ///   programmatic-admit plugins (in-process or typed OOP)
    ///   have no recorded source directory and are refused
    ///   with a structured error pointing at the
    ///   distribution-layer admit code.
    ///
    /// Every dispatch decision emits a
    /// `Happening::PluginReloadDispatched` carrying mode +
    /// outcome + origin (file_watcher / operator_gesture) so
    /// operators can observe the decision live. The
    /// reload-cleanable path additionally emits the existing
    /// `PluginLiveReloadStarted` / `PluginLiveReloadCompleted`
    /// / `PluginLiveReloadFailed` trio for the teardown +
    /// re-admit cycle.
    ///
    /// The legacy `lifecycle.hot_reload` field is parsed for
    /// backward compatibility but is no longer consulted by
    /// the actuation path; `lifecycle.mode` is the canonical
    /// dispatch source.
    pub async fn reload_plugin(
        &mut self,
        plugin_name: &str,
    ) -> Result<(), StewardError> {
        self.reload_plugin_with_source(plugin_name, "operator_gesture")
            .await
    }

    /// Like [`Self::reload_plugin`] but records the origin of
    /// the reload request in the emitted
    /// `PluginReloadDispatched` happening. Pass
    /// `"operator_gesture"` for wire-op invocations,
    /// `"file_watcher"` for inotify / polling-loop fires.
    pub async fn reload_plugin_with_source(
        &mut self,
        plugin_name: &str,
        source: &str,
    ) -> Result<(), StewardError> {
        // Look up the plugin by canonical name. The router is
        // keyed by shelf; the lookup walks the admission_order.
        let entry =
            self.router.lookup_by_name(plugin_name).ok_or_else(|| {
                self.emit_reload_dispatched(
                    plugin_name,
                    "",
                    "plugin_not_admitted",
                    source,
                );
                StewardError::Dispatch(format!(
                    "no plugin admitted with name {plugin_name}"
                ))
            })?;

        // Resolve the recorded origin. Reload-cleanable
        // requires a recorded origin to re-admit from;
        // programmatic-admit plugins have None.
        let plugin_dir = self
            .plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .get(plugin_name)
            .cloned();

        // The entry's manifest is the source of truth for the
        // lifecycle mode and plugin version. Every admit_*
        // entry point attaches the manifest at admission; the
        // permissive path used by test fixtures does not, and
        // surfaces here as a refusal with structured outcome
        // so the omission is loud rather than silent.
        let entry_manifest = entry.current_manifest().ok_or_else(|| {
            self.emit_reload_dispatched(
                plugin_name,
                "",
                "no_manifest_recorded",
                source,
            );
            StewardError::Dispatch(format!(
                "{plugin_name}: plugin entry has no recorded manifest; \
                 the admission entry point did not attach one. Reload \
                 is only available for plugins admitted via the typed \
                 admit_* entry points or admit_out_of_process_from_directory."
            ))
        })?;

        let mode = entry_manifest.require_lifecycle().mode;
        let mode_label = lifecycle_mode_label(mode);

        match mode {
            evo_plugin_sdk::manifest::LifecycleMode::Frozen => {
                self.emit_reload_dispatched(
                    plugin_name,
                    mode_label,
                    "refused_frozen",
                    source,
                );
                Err(StewardError::Dispatch(format!(
                    "{plugin_name}: lifecycle.mode = frozen; the plugin \
                     opted out of operator-config reload. Restart the \
                     steward to apply TOML changes, or update the \
                     manifest to declare a different mode."
                )))
            }
            evo_plugin_sdk::manifest::LifecycleMode::ReactiveOnly => {
                self.emit_reload_dispatched(
                    plugin_name,
                    mode_label,
                    "substrate_driven_acknowledged",
                    source,
                );
                tracing::info!(
                    plugin = plugin_name,
                    source,
                    "lifecycle dispatcher: plugin is reactive-only; \
                     operator state lives in framework substrates; \
                     no teardown invoked"
                );
                Ok(())
            }
            evo_plugin_sdk::manifest::LifecycleMode::ReloadCleanable => {
                if plugin_dir.is_none() {
                    self.emit_reload_dispatched(
                        plugin_name,
                        mode_label,
                        "no_recorded_origin",
                        source,
                    );
                    return Err(StewardError::Dispatch(format!(
                        "{plugin_name}: cannot reload; the plugin was admitted \
                         programmatically (no source directory recorded). The \
                         distribution's admit code must perform unload + admit \
                         against a fresh handle."
                    )));
                }
                let plugin_dir = plugin_dir.expect("checked above");
                let shelf = entry.shelf.clone();

                self.emit_reload_dispatched(
                    plugin_name,
                    mode_label,
                    "teardown_and_readmit_started",
                    source,
                );

                // Evict from the routing table first so the shelf
                // is free for the re-admit. Concurrent dispatches
                // see a structured "no plugin on shelf" until the
                // re-admit completes.
                let removed = self.router.remove(&shelf).ok_or_else(|| {
                    StewardError::Dispatch(format!(
                        "{plugin_name}: router lost the entry between \
                             lookup and remove"
                    ))
                })?;
                // Drop the shared lookup reference so the only
                // remaining strong owner of the Arc is `removed`.
                drop(entry);

                // Drop the recorded origin BEFORE the re-admit
                // path inserts a fresh one; otherwise a concurrent
                // observer could see two distinct origins for the
                // same plugin name during the gap.
                self.plugin_origins
                    .lock()
                    .expect("plugin_origins mutex poisoned")
                    .remove(plugin_name);

                // Release the URI schemes the old copy owned so
                // the re-admit path can register them afresh.
                self.unregister_uri_schemes_for_entry(&removed).await;

                // Unload the evicted entry. Errors are logged but
                // do not block re-admission: the goal of reload
                // is to bring the plugin back even if the old
                // copy is misbehaving.
                if let Err(e) = unload_one_plugin(removed).await {
                    tracing::warn!(
                        plugin = %plugin_name,
                        error = %e,
                        "reload: unload of old copy failed; \
                         continuing with re-admission"
                    );
                }

                let runtime_dir =
                    self.plugin_runtime_dir.clone().ok_or_else(|| {
                        StewardError::Dispatch(
                            "reload_plugin: engine constructed without a \
                         plugin runtime directory; reload-cleanable reload \
                         requires it (set via AdmissionEngine::\
                         with_plugin_runtime_dir at construction)"
                                .to_string(),
                        )
                    })?;
                // Re-admit from the same directory.
                self.admit_out_of_process_from_directory(
                    &plugin_dir,
                    &runtime_dir,
                )
                .await
            }
        }
    }

    fn emit_reload_dispatched(
        &self,
        plugin: &str,
        mode: &str,
        outcome: &str,
        source: &str,
    ) {
        let bus = self.state.bus.clone();
        let plugin = plugin.to_string();
        let mode = mode.to_string();
        let outcome = outcome.to_string();
        let source = source.to_string();
        tokio::spawn(async move {
            let _ = bus
                .emit_durable(
                    crate::happenings::Happening::PluginReloadDispatched {
                        plugin,
                        mode,
                        outcome,
                        source,
                        at: std::time::SystemTime::now(),
                    },
                )
                .await;
        });
    }

    /// Operator-issued reload of a single plugin's manifest
    /// declarations. The plugin's running instance keeps its
    /// handle, custodies, and any in-flight session through the
    /// swap; only the declarative surface (capabilities,
    /// course-correct verbs, lifecycle policy) changes.
    ///
    /// Validation pipeline (atomic-swap semantics):
    /// 1. Read the source TOML.
    /// 2. Parse + schema-validate via `Manifest::from_toml` and
    ///    `Manifest::validate`.
    /// 3. Identity check: the new manifest's `plugin.name` must
    ///    match the running plugin's recorded name.
    /// 4. Transport check: the new manifest's `transport.kind`
    ///    must match the running plugin's transport (in-process
    ///    plugins cannot be reloaded into OOP and vice versa).
    /// 5. Drift re-check: the new manifest's declared verb sets
    ///    are diffed against the running plugin's `describe()`;
    ///    drift refuses with a structured error.
    /// 6. Atomic swap (only after every check passes): the
    ///    entry's stored manifest and enforcement policy are
    ///    replaced via the entry's [`ArcSwap`] so dispatch
    ///    reads stay lock-free.
    ///
    /// `dry_run = true` runs the pipeline through validation
    /// and drift but does NOT swap state; the outcome reports
    /// the from / to versions so operators can preview.
    ///
    /// The `reason` argument is captured for audit visibility
    /// in the structured `PluginManifestReloaded` /
    /// `PluginManifestInvalid` happenings; admin-ledger
    /// receipts ride a follow-up that lands with the operator
    /// wire op.
    pub async fn reload_manifest(
        &self,
        plugin_name: &str,
        source: ManifestSource,
        dry_run: bool,
    ) -> Result<ManifestReloadOutcome, StewardError> {
        let started_at = std::time::Instant::now();

        let entry =
            self.router.lookup_by_name(plugin_name).ok_or_else(|| {
                StewardError::Dispatch(format!(
                    "no plugin admitted with name {plugin_name}"
                ))
            })?;
        let current_manifest = entry.current_manifest().ok_or_else(|| {
            StewardError::Dispatch(format!(
                "{plugin_name}: plugin entry has no recorded manifest; \
                 reload_manifest is only available for plugins admitted \
                 via the typed admit_* entry points or \
                 admit_out_of_process_from_directory."
            ))
        })?;
        let from_version = current_manifest.plugin.version.to_string();

        // Stage 1: read source.
        let toml_text = match &source {
            ManifestSource::Inline(t) => t.clone(),
            ManifestSource::Path(p) => {
                std::fs::read_to_string(p).map_err(|e| {
                    self.emit_manifest_invalid(
                        plugin_name,
                        "parse",
                        &format!("reading {}: {e}", p.display()),
                    );
                    StewardError::io(
                        format!("reading manifest source {}", p.display()),
                        e,
                    )
                })?
            }
        };

        // Stage 2: parse + schema validate.
        let new_manifest = match Manifest::from_toml(&toml_text) {
            Ok(m) => m,
            Err(e) => {
                let reason = format!("parse failed: {e}");
                self.emit_manifest_invalid(plugin_name, "parse", &reason);
                return Err(StewardError::Admission(reason));
            }
        };
        if let Err(e) = new_manifest.validate() {
            let reason = format!("schema validation failed: {e}");
            self.emit_manifest_invalid(plugin_name, "schema", &reason);
            return Err(StewardError::Admission(reason));
        }

        // Stage 3: identity check.
        if new_manifest.plugin.name != plugin_name {
            let reason = format!(
                "manifest declares plugin name {} but reload was \
                 requested for {plugin_name}",
                new_manifest.plugin.name
            );
            self.emit_manifest_invalid(plugin_name, "identity", &reason);
            return Err(StewardError::Admission(reason));
        }
        if new_manifest.plugin.kind != current_manifest.plugin.kind {
            let reason = format!(
                "manifest declares plugin.kind = {:?} but the running \
                 plugin was admitted with plugin.kind = {:?}; \
                 artefact-kind changes require unload + admit, not \
                 reload_manifest",
                new_manifest.plugin.kind, current_manifest.plugin.kind
            );
            self.emit_manifest_invalid(plugin_name, "kind", &reason);
            return Err(StewardError::Admission(reason));
        }

        // Stage 4: transport check.
        // Skipped for artefact manifests — they have no
        // transport block; the artefact-kind equality check
        // above is the relevant identity guard for them.
        if new_manifest.plugin.kind == ArtefactKind::Functional {
            let new_transport_kind = new_manifest.require_transport().kind;
            let current_transport_kind =
                current_manifest.require_transport().kind;
            if new_transport_kind != current_transport_kind {
                let reason = format!(
                    "manifest declares transport.kind = {new_transport_kind:?} \
                     but the running plugin was admitted with \
                     transport.kind = {current_transport_kind:?}; \
                     transport changes require unload + admit, not \
                     reload_manifest",
                );
                self.emit_manifest_invalid(plugin_name, "transport", &reason);
                return Err(StewardError::Admission(reason));
            }
        }

        // Stage 5: drift re-check against the running plugin's
        // describe().
        let description = {
            // describe is &self on both Plugin and Warden traits;
            // take a read guard to avoid blocking an in-flight
            // dispatch on this entry.
            let guard = entry.handle.read().await;
            match guard.as_ref() {
                Some(h) => h.describe().await,
                None => {
                    let reason = "plugin handle is unavailable; cannot \
                                  drift-check reload_manifest"
                        .to_string();
                    self.emit_manifest_invalid(plugin_name, "drift", &reason);
                    return Err(StewardError::Dispatch(format!(
                        "{plugin_name}: {reason}"
                    )));
                }
            }
        };
        let drift = evo_plugin_sdk::drift::detect_drift(
            &new_manifest,
            &description.runtime_capabilities,
        );
        if !drift.is_empty() {
            let reason = format!(
                "manifest does not match runtime describe(): \
                 missing in implementation = {:?}, missing in \
                 manifest = {:?}",
                drift.missing_in_implementation, drift.missing_in_manifest
            );
            self.emit_manifest_invalid(plugin_name, "drift", &reason);
            return Err(StewardError::Admission(reason));
        }

        let to_version = new_manifest.plugin.version.to_string();

        if dry_run {
            return Ok(ManifestReloadOutcome {
                plugin: plugin_name.to_string(),
                from_manifest_version: from_version,
                to_manifest_version: to_version,
                duration_ms: started_at.elapsed().as_millis() as u64,
                dry_run: true,
            });
        }

        // Stage 6: atomic swap. Replace the enforcement policy
        // first (dispatch sees the new gate immediately), then
        // the cached manifest (lifecycle paths see the new
        // declarations on the next reload_plugin / reload_manifest).
        let new_policy =
            crate::router::EnforcementPolicy::from_manifest(&new_manifest);
        entry.policy.store(Arc::new(new_policy));
        {
            let mut slot =
                entry.manifest.lock().expect("manifest mutex poisoned");
            *slot = Some(Arc::new(new_manifest));
        }

        let _ = self
            .state
            .bus
            .emit_durable(
                crate::happenings::Happening::PluginManifestReloaded {
                    plugin: plugin_name.to_string(),
                    from_manifest_version: from_version.clone(),
                    to_manifest_version: to_version.clone(),
                    at: std::time::SystemTime::now(),
                },
            )
            .await;

        Ok(ManifestReloadOutcome {
            plugin: plugin_name.to_string(),
            from_manifest_version: from_version,
            to_manifest_version: to_version,
            duration_ms: started_at.elapsed().as_millis() as u64,
            dry_run: false,
        })
    }

    /// Helper for [`Self::reload_manifest`]: spawn-and-forget
    /// emit of `PluginManifestInvalid`. Invoked from the sync
    /// stage-error paths, then the caller returns the
    /// structured error to the operator. The fire-and-forget is
    /// acceptable because the bus's emit_durable persists to
    /// the happenings_log before resolving; a dropped future
    /// here means the durable write may not complete, but the
    /// operator-visible outcome (the structured error returned)
    /// is unchanged.
    fn emit_manifest_invalid(
        &self,
        plugin_name: &str,
        stage: &str,
        reason: &str,
    ) {
        let bus = Arc::clone(&self.state.bus);
        let plugin = plugin_name.to_string();
        let stage = stage.to_string();
        let reason = reason.to_string();
        tokio::spawn(async move {
            let _ = bus
                .emit_durable(
                    crate::happenings::Happening::PluginManifestInvalid {
                        plugin,
                        stage,
                        reason,
                        at: std::time::SystemTime::now(),
                    },
                )
                .await;
        });
    }

    /// Operator-issued reload of the catalogue declarations.
    /// Replaces the framework's loaded rack / shelf / type /
    /// relation-predicate vocabulary atomically; admission paths
    /// see the new declarations on their next call.
    ///
    /// Validation pipeline (atomic-swap semantics):
    /// 1. Read source TOML.
    /// 2. Parse + schema-validate via `Catalogue::from_toml`
    ///    (the parser refuses unknown fields, missing required
    ///    fields, and out-of-range schema versions).
    /// 3. Shelf-occupancy re-check: every currently-admitted
    ///    plugin's shelf must still exist in the new catalogue
    ///    with a compatible shape. A removed shelf for a
    ///    running plugin emits a `CardinalityViolation` happening
    ///    naming the shelf and refuses the reload (plugin
    ///    re-admission for declaration changes is operator-issued
    ///    via `reload_plugin`; the framework refuses to silently
    ///    orphan a running plugin).
    /// 4. Atomic swap (only after every check passes): the
    ///    state's catalogue [`arc_swap::ArcSwap`] is updated so
    ///    subsequent admission paths see the new declarations.
    ///
    /// `dry_run = true` runs the pipeline through validation
    /// without mutating state; the outcome reports the from / to
    /// schema versions and rack counts for operator preview.
    ///
    /// Cardinality re-check against existing storage state
    /// (subjects-per-shelf vs new declared maxima), automatic
    /// plugin re-admission, and LKG shadow-file write on success
    /// land in follow-up commits; today's primitive holds the
    /// declaration-vocabulary contract atomic and refuses any
    /// reload that would orphan a running plugin.
    pub async fn reload_catalogue(
        &self,
        source: ManifestSource,
        dry_run: bool,
    ) -> Result<CatalogueReloadOutcome, StewardError> {
        let started_at = std::time::Instant::now();

        // Stage 1: read source.
        let toml_text = match &source {
            ManifestSource::Inline(t) => t.clone(),
            ManifestSource::Path(p) => {
                std::fs::read_to_string(p).map_err(|e| {
                    self.emit_catalogue_invalid(
                        "parse",
                        &format!("reading {}: {e}", p.display()),
                    );
                    StewardError::io(
                        format!("reading catalogue source {}", p.display()),
                        e,
                    )
                })?
            }
        };

        // Stage 2: parse + schema validate. Catalogue::from_toml
        // returns StewardError directly on parse / schema failures;
        // reuse its diagnostic verbatim and surface it through the
        // structured happening.
        let new_catalogue =
            match crate::catalogue::Catalogue::from_toml(&toml_text) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    let reason = e.to_string();
                    self.emit_catalogue_invalid("parse", &reason);
                    return Err(e);
                }
            };

        let current = self.state.current_catalogue();
        let from_schema = current.schema_version;
        let to_schema = new_catalogue.schema_version;

        // Stage 3: shelf-occupancy re-check. Every currently-
        // admitted plugin's shelf must still exist in the new
        // catalogue with a compatible shape; otherwise the
        // reload is refused.
        let mut conflicts: Vec<(String, String)> = Vec::new();
        for entry in self.router.entries_in_order() {
            let shelf_qualified = entry.shelf.clone();
            match new_catalogue.find_shelf(&shelf_qualified) {
                None => {
                    let reason = format!(
                        "plugin {} occupies shelf {} which the new \
                         catalogue does not declare",
                        entry.name, shelf_qualified
                    );
                    self.emit_cardinality_violation(&shelf_qualified, &reason);
                    conflicts.push((shelf_qualified, reason));
                }
                Some(new_shelf) => {
                    if let Some(manifest) = entry.current_manifest() {
                        if !new_shelf.accepts_shape(manifest.target.shape) {
                            let reason = format!(
                                "plugin {} occupies shelf {} with shape {} \
                                 but the new catalogue declares shape {} \
                                 for that shelf",
                                entry.name,
                                shelf_qualified,
                                manifest.target.shape,
                                new_shelf.shape
                            );
                            self.emit_cardinality_violation(
                                &shelf_qualified,
                                &reason,
                            );
                            conflicts.push((shelf_qualified, reason));
                        }
                    }
                }
            }
        }
        if !conflicts.is_empty() {
            let summary = conflicts
                .iter()
                .map(|(s, _)| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let reason = format!(
                "{} shelf-occupancy conflict(s) refuse the reload: {}",
                conflicts.len(),
                summary
            );
            self.emit_catalogue_invalid("shelf_in_use", &reason);
            return Err(StewardError::Admission(reason));
        }

        let rack_count = new_catalogue.racks.len() as u32;

        if dry_run {
            return Ok(CatalogueReloadOutcome {
                from_schema_version: from_schema,
                to_schema_version: to_schema,
                rack_count,
                duration_ms: started_at.elapsed().as_millis() as u64,
                dry_run: true,
            });
        }

        // Stage 4: atomic swap.
        self.state.catalogue.store(Arc::clone(&new_catalogue));

        // Stage 5: re-run the orphan diagnostic against the new
        // catalogue's declared subject types. A reload that
        // removes a subject_type declaration without first
        // migrating the persisted rows of that type produces
        // exactly the same operator-visible state as a fresh
        // boot under that catalogue: pending_grammar_orphans
        // upserted, SubjectGrammarOrphan happenings emitted.
        // Without this stage T3.orphan-reload (and any future
        // hot-catalogue-evolution flow) cannot observe orphans
        // until the next steward restart.
        let declared_types: std::collections::HashSet<String> = new_catalogue
            .subjects
            .iter()
            .map(|s| s.name.clone())
            .collect();
        crate::grammar_migration::scan_grammar_orphans(
            &self.state.persistence,
            &self.state.bus,
            &declared_types,
        )
        .await;

        let _ = self
            .state
            .bus
            .emit_durable(crate::happenings::Happening::CatalogueReloaded {
                from_schema_version: from_schema,
                to_schema_version: to_schema,
                rack_count,
                at: std::time::SystemTime::now(),
            })
            .await;

        Ok(CatalogueReloadOutcome {
            from_schema_version: from_schema,
            to_schema_version: to_schema,
            rack_count,
            duration_ms: started_at.elapsed().as_millis() as u64,
            dry_run: false,
        })
    }

    /// Helper for [`Self::reload_catalogue`]: spawn-and-forget
    /// emit of `CatalogueInvalid`. The fire-and-forget rationale
    /// matches [`Self::emit_manifest_invalid`].
    fn emit_catalogue_invalid(&self, stage: &str, reason: &str) {
        let bus = Arc::clone(&self.state.bus);
        let stage = stage.to_string();
        let reason = reason.to_string();
        tokio::spawn(async move {
            let _ = bus
                .emit_durable(crate::happenings::Happening::CatalogueInvalid {
                    stage,
                    reason,
                    at: std::time::SystemTime::now(),
                })
                .await;
        });
    }

    /// Helper for [`Self::reload_catalogue`]: spawn-and-forget
    /// emit of `CardinalityViolation`. Emitted per offending
    /// shelf; the caller aggregates and returns a single
    /// structured error.
    fn emit_cardinality_violation(&self, shelf: &str, reason: &str) {
        let bus = Arc::clone(&self.state.bus);
        let shelf = shelf.to_string();
        let reason = reason.to_string();
        tokio::spawn(async move {
            let _ = bus
                .emit_durable(
                    crate::happenings::Happening::CardinalityViolation {
                        shelf,
                        reason,
                        at: std::time::SystemTime::now(),
                    },
                )
                .await;
        });
    }

    /// Operator-issued enable: persists the `installed_plugins`
    /// row with `enabled = true` and records the operator-supplied
    /// reason and timestamp. Inline re-admission of a currently-
    /// unloaded plugin is staged behind the next-discovery
    /// boundary in this build; today's verb sets the bit and the
    /// next discovery pass admits the bundle. `was_currently_admitted`
    /// lets the caller render the "already running" outcome
    /// distinctly from "now enabled, will admit at next discovery".
    pub async fn enable_plugin(
        &self,
        plugin_name: &str,
        reason: Option<String>,
    ) -> Result<PluginLifecycleOutcome, StewardError> {
        let now_ms = system_time_now_ms();
        let was_currently_admitted =
            self.router.lookup_by_name(plugin_name).is_some();

        // Preserve any prior install_digest so consecutive
        // enable / disable cycles don't lose the audit pin. A
        // first-time enable on a plugin discovery hasn't yet
        // recorded gets an empty digest until a future commit
        // computes the bundle digest at admission time.
        let prior_digest = self
            .state
            .persistence
            .load_all_installed_plugins()
            .await?
            .into_iter()
            .find(|r| r.plugin_name == plugin_name)
            .map(|r| r.install_digest)
            .unwrap_or_default();

        let row = crate::persistence::PersistedInstalledPlugin {
            plugin_name: plugin_name.to_string(),
            enabled: true,
            last_state_reason: reason,
            last_state_changed_at_ms: now_ms,
            install_digest: prior_digest,
        };
        self.state.persistence.record_plugin_enabled(&row).await?;
        Ok(PluginLifecycleOutcome {
            plugin: plugin_name.to_string(),
            was_currently_admitted,
            change_applied: true,
        })
    }

    /// Operator-issued disable: drains the running plugin if
    /// admitted, then persists `enabled = false`. Refuses if the
    /// plugin occupies a catalogue shelf declared `required =
    /// true` and is the only occupant.
    pub async fn disable_plugin(
        &self,
        plugin_name: &str,
        reason: Option<String>,
    ) -> Result<PluginLifecycleOutcome, StewardError> {
        // Essentialness check via the catalogue's required flag
        // applied to the running plugin's shelf.
        if let Some(entry) = self.router.lookup_by_name(plugin_name) {
            self.refuse_if_essential(&entry, "disable")?;
        }

        let now_ms = system_time_now_ms();
        let was_currently_admitted =
            self.router.lookup_by_name(plugin_name).is_some();

        // Drain the entry if currently admitted.
        if was_currently_admitted {
            let entry =
                self.router.lookup_by_name(plugin_name).ok_or_else(|| {
                    StewardError::Dispatch(format!(
                        "{plugin_name}: router lost the entry between \
                         lookup and disable"
                    ))
                })?;
            let shelf = entry.shelf.clone();
            drop(entry);
            self.plugin_origins
                .lock()
                .expect("plugin_origins mutex poisoned")
                .remove(plugin_name);
            if let Some(removed) = self.router.remove(&shelf) {
                self.unregister_uri_schemes_for_entry(&removed).await;
                if let Err(e) = unload_one_plugin(removed).await {
                    tracing::warn!(
                        plugin = %plugin_name,
                        error = %e,
                        "disable_plugin: drain reported error; proceeding \
                         with persistence write"
                    );
                }
            }
        }

        let prior_digest = self
            .state
            .persistence
            .load_all_installed_plugins()
            .await?
            .into_iter()
            .find(|r| r.plugin_name == plugin_name)
            .map(|r| r.install_digest)
            .unwrap_or_default();

        let row = crate::persistence::PersistedInstalledPlugin {
            plugin_name: plugin_name.to_string(),
            enabled: false,
            last_state_reason: reason,
            last_state_changed_at_ms: now_ms,
            install_digest: prior_digest,
        };
        self.state.persistence.record_plugin_enabled(&row).await?;
        Ok(PluginLifecycleOutcome {
            plugin: plugin_name.to_string(),
            was_currently_admitted,
            change_applied: true,
        })
    }

    /// Operator-issued uninstall: drains the running plugin,
    /// removes the bundle directory from disk (when the plugin
    /// was admitted from a recorded directory), and forgets the
    /// `installed_plugins` row. Refuses if the plugin is
    /// essential. When `purge_state = true`, the per-plugin
    /// `state/` and `credentials/` directories are wiped after
    /// the bundle is removed.
    pub async fn uninstall_plugin(
        &self,
        plugin_name: &str,
        reason: Option<String>,
        purge_state: bool,
    ) -> Result<PluginLifecycleOutcome, StewardError> {
        // Dispatch by kind: artefact plugins live in the
        // artefact registries (not the router). The router-
        // driven uninstall path below handles only
        // functional plugins; artefact-kind names go
        // through the dedicated unadmit_artefact path which
        // unregisters from the matching registry, auto-
        // clears any active-selection slot pointing at the
        // plugin, and emits the corresponding Ui*Unadmitted
        // happening.
        if let Some(_kind) = self.lookup_artefact_kind(plugin_name).await {
            // Artefact path. Capture the recorded bundle
            // directory BEFORE unadmit_artefact clears the
            // plugin_origins entry; we still need the path
            // to remove the bundle from disk.
            let plugin_dir = self
                .plugin_origins
                .lock()
                .expect("plugin_origins mutex poisoned")
                .get(plugin_name)
                .cloned();
            self.unadmit_artefact(plugin_name).await?;

            if let Some(dir) = plugin_dir {
                if let Err(e) = std::fs::remove_dir_all(&dir) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Err(StewardError::io(
                            format!(
                                "removing artefact bundle {} for \
                                 uninstall",
                                dir.display()
                            ),
                            e,
                        ));
                    }
                }
            }

            if purge_state {
                self.purge_plugin_state(plugin_name).await?;
            }

            // Drop the persistence row. Today the artefact
            // admit paths don't write installed_plugins
            // rows; the forget call is a no-op for absent
            // rows but is included for symmetry with the
            // functional path so future extension that
            // tracks artefacts in installed_plugins remains
            // single-call-site.
            self.state
                .persistence
                .forget_installed_plugin(plugin_name)
                .await?;

            let _ = reason;
            return Ok(PluginLifecycleOutcome {
                plugin: plugin_name.to_string(),
                was_currently_admitted: true,
                change_applied: true,
            });
        }

        if let Some(entry) = self.router.lookup_by_name(plugin_name) {
            self.refuse_if_essential(&entry, "uninstall")?;
        }

        let was_currently_admitted =
            self.router.lookup_by_name(plugin_name).is_some();

        // Drain (uninstall implies disable).
        if was_currently_admitted {
            self.disable_plugin(plugin_name, reason.clone()).await?;
        }

        // Remove the bundle directory recorded as the plugin's
        // origin. In-process plugins have no recorded directory;
        // their uninstall is a no-op at the bundle layer (the
        // distribution removes the binary itself).
        let plugin_dir = self
            .plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .get(plugin_name)
            .cloned();
        if let Some(dir) = plugin_dir {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(StewardError::io(
                        format!(
                            "removing plugin bundle {} for uninstall",
                            dir.display()
                        ),
                        e,
                    ));
                }
            }
        }

        if purge_state {
            self.purge_plugin_state(plugin_name).await?;
        }

        // Remove the persistence row last so the audit trail
        // outlives the plugin's identity until disk cleanup
        // completes.
        self.state
            .persistence
            .forget_installed_plugin(plugin_name)
            .await?;

        // Drop the plugin from the gateway registry if it
        // was a gateway. Idempotent on absent entries.
        self.unregister_gateway(plugin_name).await;

        // Drop the plugin's UI stockings recording.
        // Idempotent on absent entries (no-op when no UI
        // surface was admitted for the plugin).
        self.forget_ui_stockings(plugin_name).await;

        Ok(PluginLifecycleOutcome {
            plugin: plugin_name.to_string(),
            was_currently_admitted,
            change_applied: true,
        })
    }

    /// Operator-issued state purge: wipes the plugin's `state/`
    /// and `credentials/` directories without removing the
    /// bundle itself. Used when an operator wants to "factory
    /// reset" a plugin while keeping the code installed.
    pub async fn purge_plugin_state(
        &self,
        plugin_name: &str,
    ) -> Result<PluginLifecycleOutcome, StewardError> {
        let was_currently_admitted =
            self.router.lookup_by_name(plugin_name).is_some();
        for sub in ["state", "credentials"] {
            let p = self.plugin_data_root.join(plugin_name).join(sub);
            if p.exists() {
                std::fs::remove_dir_all(&p).map_err(|e| {
                    StewardError::io(
                        format!(
                            "purging plugin {} dir {}",
                            plugin_name,
                            p.display()
                        ),
                        e,
                    )
                })?;
            }
            // Recreate the empty directory so subsequent admission
            // paths find a writable structure.
            std::fs::create_dir_all(&p).map_err(|e| {
                StewardError::io(format!("recreating {}", p.display()), e)
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&p) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o700);
                    let _ = std::fs::set_permissions(&p, perms);
                }
            }
        }
        Ok(PluginLifecycleOutcome {
            plugin: plugin_name.to_string(),
            was_currently_admitted,
            change_applied: true,
        })
    }

    /// Helper for [`Self::disable_plugin`] and
    /// [`Self::uninstall_plugin`]: refuse if the plugin occupies
    /// a catalogue shelf declared `required = true` and is the
    /// only occupant. Returns `Ok(())` when the action is
    /// permitted.
    fn refuse_if_essential(
        &self,
        entry: &Arc<PluginEntry>,
        verb: &str,
    ) -> Result<(), StewardError> {
        let catalogue = self.state.current_catalogue();
        if let Some(shelf) = catalogue.find_shelf(&entry.shelf) {
            if shelf.required {
                // Every shelf is single-occupant today, so a
                // required shelf with an admitted plugin always
                // refuses the action. Multi-occupant shelves are
                // a future extension of the catalogue model.
                return Err(StewardError::Admission(format!(
                    "{}: cannot {verb}; shelf {} is declared \
                     required = true in the catalogue",
                    entry.name, entry.shelf
                )));
            }
        }
        Ok(())
    }

    /// Run a health check against every admitted plugin, returning a
    /// vector of (plugin name, report) pairs.
    pub async fn health_check_all(&self) -> Vec<(String, HealthReport)> {
        self.router.health_check_all().await
    }

    /// Gracefully unload every admitted plugin under default shutdown
    /// timing. Errors are logged but do not propagate; clean and killed
    /// plugin counts are tracked via [`Self::shutdown_with_config`] but
    /// discarded by this convenience wrapper for backward compatibility.
    ///
    /// This delegates to [`Self::shutdown_with_config`] using
    /// [`ShutdownConfig::default`] (10-second global deadline). Callers
    /// that want the per-plugin outcome should call
    /// `shutdown_with_config` directly and inspect the returned
    /// [`ShutdownReport`].
    pub async fn shutdown(&self) -> Result<(), StewardError> {
        let _report =
            self.shutdown_with_config(ShutdownConfig::default()).await;
        Ok(())
    }

    /// Drain every admitted plugin under a single global deadline,
    /// returning a [`ShutdownReport`] describing which plugins
    /// unloaded cleanly and which were killed after the deadline.
    ///
    /// Stages, in order:
    ///
    /// 1. **Stop new connections.** The server's accept loop is
    ///    expected to have exited before this call; this method
    ///    does not directly interact with it.
    /// 2. **Drain custody.** Every active warden custody recorded
    ///    in the [`CustodyLedger`] is released via the router's
    ///    `release_custody` verb, with a bounded sub-deadline of
    ///    `min(2s, global_deadline / 4)`. Custodies that release
    ///    cleanly within the window appear under
    ///    [`ShutdownReport::custody_drained`]; those that time out
    ///    or error appear under [`ShutdownReport::custody_abandoned`].
    /// 3. **Parallel plugin unload.** Each admitted plugin is
    ///    drained from the router; one [`tokio::spawn`] task per
    ///    plugin runs the existing per-plugin unload sequence
    ///    (`unload()` over the wire / in-process, then drop the
    ///    handle, then reap the child process for out-of-process
    ///    plugins). All tasks share a single global deadline.
    /// 4. **SIGKILL holdouts.** Any out-of-process plugin whose
    ///    unload task is still running when the global deadline
    ///    elapses has its task aborted and its child process
    ///    killed. The plugin's name is recorded in
    ///    [`ShutdownReport::plugins_killed_after_deadline`].
    /// 5. **Persistence flush.** Reserved for future persistence
    ///    work; currently a no-op.
    /// 6. **Return.** The report is returned to the caller for
    ///    structured logging or audit.
    pub async fn shutdown_with_config(
        &self,
        config: ShutdownConfig,
    ) -> ShutdownReport {
        let started_at = Instant::now();

        // Stage 1: drain factory-announced instances. Each registered
        // factory announcer walks its instance map and emits one
        // `FactoryInstanceRetracted` happening per instance, removing
        // the underlying subject. Done before plugin unload so
        // subscribers see the lifecycle reverse the announce-order
        // and so any in-flight projection invalidations resolve
        // before the plugin processes go away.
        let factory_drain_summary = self.drain_factory_instances().await;
        if factory_drain_summary.total > 0 {
            tracing::info!(
                factories = factory_drain_summary.factories,
                instances_retracted = factory_drain_summary.total,
                instances_errored = factory_drain_summary.errored,
                "factory-instance drain complete"
            );
        }

        // Stage 2: custody drain. Bounded by min(2s, deadline / 4).
        let custody_window =
            std::cmp::min(Duration::from_secs(2), config.global_deadline / 4);
        let (custody_drained, custody_abandoned) =
            drain_active_custodies(&self.router, custody_window).await;

        // Stage 3: drain plugins from the router and unload in parallel.
        // The wire-unload path is best-effort: under
        // KillMode=control-group the plugin processes share the
        // service cgroup and receive SIGTERM at the same instant
        // as the steward, so by the time this stage runs the
        // wire connection may already be torn down. Every
        // unload error path is logged inside unload_one_plugin
        // and surfaces via the report's clean / killed counts.
        let entries = self.router.drain_in_reverse_admission_order();
        let plugins_total = entries.len();
        let plugin_names: Vec<String> =
            entries.iter().map(|e| e.name.clone()).collect();

        // Release every URI scheme owned by the draining plugins
        // before the unload tasks fan out. Sequential because
        // unregister is short, and releasing before the parallel
        // unload means a fresh boot starts from a clean registry
        // even if the unload tasks are SIGKILLed past the
        // deadline.
        for entry in &entries {
            self.unregister_uri_schemes_for_entry(entry).await;
        }

        let (plugins_unloaded_cleanly, plugins_killed_after_deadline) =
            parallel_unload_with_deadline(entries, config.global_deadline)
                .await;

        // Stage 4: subject-claim sweep. The framework retracts every
        // addressing each departing plugin claimed in the subject
        // registry. Runs after wire-unload because a graceful
        // unload may have retracted some claims via the plugin's
        // own unload path; the sweep finds anything that survived
        // (the common case under KillMode=control-group, where
        // the plugin process exits before its unload() runs)
        // and retracts them on the plugin's behalf so the
        // durable subjects table, the in-memory registry, the
        // happenings bus, and the conflict index all reflect the
        // departure consistently. Without this sweep, every
        // restart leaves orphaned subjects that surface as
        // catalogue-orphan diagnostics on the next boot.
        let claims_swept =
            drain_plugin_subject_claims(&self.state, &plugin_names).await;
        if claims_swept > 0 {
            tracing::info!(
                claims_swept,
                plugins_in_drain = plugin_names.len(),
                "subject-claim drain complete"
            );
        }

        // Stage 5: persistence flush. Persistence stores are not in
        // this branch; future work will flush bounded queues here
        // (e.g. happenings to disk, custody ledger snapshot to disk)
        // before stage 6 returns.
        //
        // No-op intentionally; integration point.

        let elapsed = started_at.elapsed();

        ShutdownReport {
            plugins_total,
            plugins_unloaded_cleanly,
            plugins_killed_after_deadline,
            custody_drained,
            custody_abandoned,
            elapsed,
        }
    }

    /// Walk every persisted factory-instance subject and forget any
    /// whose owning plugin has not re-announced it since boot.
    ///
    /// Called by `evo::run` after the admission setup completes plus
    /// the operator-configured grace window
    /// (`[plugins.factory_orphan_grace_secs]`, default 30 seconds).
    /// During the grace window, factory plugins admit and re-announce
    /// their instances per their `RetractionPolicy`. After the
    /// window, any persisted subject under the
    /// `evo-factory-instance` addressing scheme NOT present in any
    /// registered factory announcer's live map is an orphan: its
    /// owning plugin either did not re-admit, or admitted but did
    /// not re-announce that particular instance. The scrub walks the
    /// orphans and retracts each through the same path a plugin
    /// would use itself, emitting `Happening::SubjectForgotten` and
    /// writing the durable forget record.
    ///
    /// Returns a `ScrubReport` summarising orphans forgotten and
    /// retracts that errored. Errors during individual retracts are
    /// logged at warn level and counted; the scrub does not abort on
    /// the first error.
    pub async fn scrub_factory_orphans(&self) -> ScrubReport {
        // Collect the set of canonical IDs every registered factory
        // announcer currently knows about. Anything outside this set
        // that lives under the factory-instance addressing scheme is
        // an orphan.
        let alive_canonical_ids: std::collections::HashSet<String> = {
            let guard = self
                .factory_announcers
                .lock()
                .expect("factory announcers mutex poisoned");
            guard
                .values()
                .flat_map(|a| a.snapshot_instances())
                .map(|(_id, canonical)| canonical)
                .collect()
        };

        // Snapshot every subject currently in the registry, then
        // partition by "has at least one factory-scheme addressing".
        let subjects = self.state.subjects.snapshot_subjects();
        let mut orphans: Vec<(String, String, String)> = Vec::new();
        for s in subjects {
            for addr in &s.addressings {
                if addr.addressing.scheme
                    == crate::factory::FACTORY_INSTANCE_SCHEME
                    && !alive_canonical_ids.contains(&s.id)
                {
                    // Parse `<plugin>/<instance_id>` to recover the
                    // claimant the original announce used. The
                    // recovered plugin name is what the registry
                    // requires to match for retract.
                    let value = &addr.addressing.value;
                    let plugin = value
                        .split_once('/')
                        .map(|(p, _)| p.to_string())
                        .unwrap_or_else(|| addr.claimant.clone());
                    orphans.push((s.id.clone(), plugin, value.clone()));
                    break; // one orphan per subject; further factory
                           // addressings on the same subject share its
                           // fate and the registry retract handles
                           // them through the cascade.
                }
            }
        }

        if orphans.is_empty() {
            return ScrubReport {
                forgotten: 0,
                errored: 0,
            };
        }

        let mut forgotten = 0;
        let mut errored = 0;
        for (canonical_id, plugin, value) in orphans {
            let addressing = evo_plugin_sdk::contract::ExternalAddressing::new(
                crate::factory::FACTORY_INSTANCE_SCHEME,
                value,
            );
            // Use the wiring-layer subject announcer so the retract
            // path emits durable `Happening::SubjectForgotten` and
            // records the durable `subject_forget`. The plugin_name
            // on the synthesised announcer matches the original
            // claimant recorded with the addressing, so the
            // registry permits the retract.
            let announcer = crate::context::RegistrySubjectAnnouncer::new(
                Arc::clone(&self.state.subjects),
                Arc::clone(&self.state.relations),
                self.state.current_catalogue(),
                Arc::clone(&self.state.bus),
                plugin.clone(),
            )
            .with_persistence(Arc::clone(&self.state.persistence))
            .with_conflict_index(Arc::clone(&self.state.conflict_index));

            use evo_plugin_sdk::contract::SubjectAnnouncer;
            match announcer
                .retract(
                    addressing,
                    Some(
                        "factory-orphan grace expired; instance not \
                         re-announced after restart"
                            .into(),
                    ),
                )
                .await
            {
                Ok(()) => {
                    forgotten += 1;
                    tracing::info!(
                        plugin = %plugin,
                        canonical_id = %canonical_id,
                        "factory orphan forgotten after grace window"
                    );
                }
                Err(e) => {
                    errored += 1;
                    tracing::warn!(
                        plugin = %plugin,
                        canonical_id = %canonical_id,
                        error = %e,
                        "factory orphan scrub: retract refused; continuing"
                    );
                }
            }
        }
        ScrubReport { forgotten, errored }
    }

    /// Walk every registered factory announcer and retract its
    /// announced instances via the bypass-policy drain method. Used
    /// by [`Self::shutdown_with_config`] as stage 1 of the shutdown
    /// pipeline; idempotent (a second call retracts nothing because
    /// the first emptied each announcer's instance map).
    ///
    /// Returns a summary: number of factories drained, total
    /// instances retracted, total instances that errored. Errors are
    /// logged at warn-level by the announcer; the caller MAY ignore
    /// them or surface them in the shutdown report.
    async fn drain_factory_instances(&self) -> FactoryDrainSummary {
        let announcers: Vec<Arc<RegistryInstanceAnnouncer>> = {
            let guard = self
                .factory_announcers
                .lock()
                .expect("factory announcers mutex poisoned");
            guard.values().cloned().collect()
        };
        let factories = announcers.len();
        let mut total = 0;
        let mut errored = 0;
        for announcer in announcers {
            let (r, e) = announcer.retract_all_for_drain().await;
            total += r;
            errored += e;
        }
        FactoryDrainSummary {
            factories,
            total,
            errored,
        }
    }
}

/// Compute the list of [`Happening::UiShelfChanged`] events
/// that describe the per-shelf transition for `plugin_name`
/// between the pre-validation `before` recording and the
/// post-validation `after` recording. Pure — separated from
/// the bus emission so the classification rules are unit-
/// testable without a runtime bus.
///
/// Classification:
///
/// - `before == 0 && after > 0` ⇒ `Stocked`.
/// - `before > 0 && after == 0` ⇒ `Withdrawn`.
/// - `before > 0 && after > 0` ⇒ `Restocked` (unconditionally
///   — operators see one event per re-admission so reactive
///   surfaces re-render on every reload, even when the
///   stocking content is identical).
fn compute_ui_shelf_changes(
    plugin_name: &str,
    before: &[evo_plugin_sdk::ui::UiStocking],
    after: &[evo_plugin_sdk::ui::UiStocking],
) -> Vec<crate::happenings::Happening> {
    use std::collections::BTreeMap;

    let mut counts: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for s in before {
        counts.entry(s.ui_shelf.clone()).or_default().0 += 1;
    }
    for s in after {
        counts.entry(s.ui_shelf.clone()).or_default().1 += 1;
    }
    let now = std::time::SystemTime::now();
    let mut out = Vec::with_capacity(counts.len());
    for (shelf, (b, a)) in counts {
        let change = match (b, a) {
            (0, after_n) if after_n > 0 => {
                crate::happenings::UiShelfChange::Stocked
            }
            (before_n, 0) if before_n > 0 => {
                crate::happenings::UiShelfChange::Withdrawn
            }
            _ => crate::happenings::UiShelfChange::Restocked,
        };
        out.push(crate::happenings::Happening::UiShelfChanged {
            shelf,
            plugin: plugin_name.to_string(),
            change,
            stockings_after: a,
            at: now,
        });
    }
    out
}

/// Emit per-shelf change happenings on the durable bus.
/// Thin wrapper around [`compute_ui_shelf_changes`] so the
/// classification rules are unit-testable in isolation.
async fn emit_ui_shelf_changes(
    bus: &Arc<crate::happenings::HappeningBus>,
    plugin_name: &str,
    before: &[evo_plugin_sdk::ui::UiStocking],
    after: &[evo_plugin_sdk::ui::UiStocking],
) {
    for h in compute_ui_shelf_changes(plugin_name, before, after) {
        let _ = bus.emit_durable(h).await;
    }
}

/// Source for the new manifest in
/// [`AdmissionEngine::reload_manifest`].
///
/// The configured-source variant (re-read the originally-loaded
/// manifest path) lands with the operator wire op in a follow-up
/// commit; today's framework primitive accepts inline TOML or an
/// arbitrary path so distributions can wire their own operator
/// surface in the meantime.
#[derive(Debug, Clone)]
pub enum ManifestSource {
    /// Manifest TOML supplied verbatim by the caller.
    Inline(String),
    /// Path read at reload time. Relative paths resolve against
    /// the steward's current working directory; orchestration
    /// tooling is expected to pass absolute paths.
    Path(std::path::PathBuf),
}

/// Outcome of a successful (or dry-run) call to
/// [`AdmissionEngine::reload_manifest`].
#[derive(Debug, Clone)]
pub struct ManifestReloadOutcome {
    /// Canonical name of the plugin whose manifest was reloaded.
    pub plugin: String,
    /// Manifest version before the reload.
    pub from_manifest_version: String,
    /// Manifest version after the reload (or the version that
    /// would have been swapped in for `dry_run = true`).
    pub to_manifest_version: String,
    /// Wall-clock duration of the validation pipeline plus the
    /// atomic swap (or just the pipeline, for dry runs).
    pub duration_ms: u64,
    /// `true` when the call ran in dry-run mode and no state
    /// was mutated.
    pub dry_run: bool,
}

/// Wall-clock millisecond timestamp suitable for the
/// `last_state_changed_at_ms` column on `installed_plugins`.
fn system_time_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Outcome of an operator-issued plugin lifecycle verb
/// ([`AdmissionEngine::enable_plugin`],
/// [`AdmissionEngine::disable_plugin`],
/// [`AdmissionEngine::uninstall_plugin`],
/// [`AdmissionEngine::purge_plugin_state`]). The shape is
/// deliberately uniform so wire serialisation maps to a single
/// reply struct on the operator surface.
#[derive(Debug, Clone)]
pub struct PluginLifecycleOutcome {
    /// Canonical name of the plugin the verb targeted.
    pub plugin: String,
    /// `true` when the plugin was admitted on the router at the
    /// moment the verb fired. Lets the operator surface render
    /// the "was running, drained" outcome distinctly from the
    /// "was already unloaded, bit flipped" outcome.
    pub was_currently_admitted: bool,
    /// `true` when the verb actually mutated state (persisted a
    /// changed bit, removed a bundle, drained an entry). Today
    /// the verbs always return `true`; a future hash-skip on
    /// no-op flips populates this with `false` without breaking
    /// the wire shape.
    pub change_applied: bool,
}

/// Outcome of a successful (or dry-run) call to
/// [`AdmissionEngine::reload_catalogue`].
#[derive(Debug, Clone)]
pub struct CatalogueReloadOutcome {
    /// Catalogue schema version before the reload.
    pub from_schema_version: u32,
    /// Catalogue schema version after the reload (or the
    /// version that would have been swapped in for
    /// `dry_run = true`).
    pub to_schema_version: u32,
    /// Number of racks in the reloaded catalogue.
    pub rack_count: u32,
    /// Wall-clock duration of the validation pipeline plus the
    /// atomic swap (or just the pipeline, for dry runs).
    pub duration_ms: u64,
    /// `true` when the call ran in dry-run mode and no state
    /// was mutated.
    pub dry_run: bool,
}

/// Outcome of [`AdmissionEngine::scrub_factory_orphans`]. Returned to
/// the caller (typically `evo::run`'s grace-window task) so the
/// caller can surface a structured log line and decide whether to
/// alert on errored retracts.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScrubReport {
    /// Number of orphans successfully forgotten.
    pub forgotten: usize,
    /// Number of orphans whose retract errored. Logged at warn level
    /// by the scrub itself; counted here for callers that want a
    /// structured summary.
    pub errored: usize,
}

/// Summary returned by `AdmissionEngine::drain_factory_instances`.
/// Diagnostic only; not part of the public `ShutdownReport`.
#[derive(Debug, Clone, Copy, Default)]
struct FactoryDrainSummary {
    /// Number of registered factory plugins whose announcers were
    /// drained (regardless of how many instances each had).
    factories: usize,
    /// Total instances retracted across all factories.
    total: usize,
    /// Total instances whose retract returned an error from the
    /// registry. Logged but not surfaced.
    errored: usize,
}

/// Configuration for [`AdmissionEngine::shutdown_with_config`].
///
/// Carries the global deadline within which every plugin must finish
/// its `unload()` and reap its child process; plugins still alive
/// after the deadline are forcibly killed.
#[derive(Debug, Clone, Copy)]
pub struct ShutdownConfig {
    /// Wall-clock budget for the entire parallel-unload stage. Default
    /// 10 seconds. The per-plugin SIGTERM-then-SIGKILL window inside
    /// the spawned task (`CHILD_SHUTDOWN_TIMEOUT`, 5 s) is no longer the
    /// dominant bound because tasks run in parallel; this deadline is
    /// the wall-clock cap.
    pub global_deadline: Duration,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            global_deadline: Duration::from_secs(10),
        }
    }
}

/// Outcome of a shutdown pass.
///
/// Returned by [`AdmissionEngine::shutdown_with_config`]. Callers log
/// it (info-level for cleanly drained, warn-level for the killed and
/// abandoned lists) so the operator audit trail records exactly which
/// plugins missed the deadline.
#[derive(Debug, Clone, Default)]
pub struct ShutdownReport {
    /// Total number of plugins that were admitted at the start of
    /// the shutdown pass.
    pub plugins_total: usize,
    /// Names of plugins that completed `unload()` and reaped their
    /// child cleanly within the global deadline.
    pub plugins_unloaded_cleanly: Vec<String>,
    /// Names of plugins whose unload task did not complete within
    /// the global deadline; these had their tasks aborted and their
    /// children SIGKILL'd. Out-of-process plugins are the typical
    /// occupants here.
    pub plugins_killed_after_deadline: Vec<String>,
    /// Active custodies that responded to release within the
    /// custody-drain sub-window. Stored as
    /// `(plugin_name, handle_id, shelf)` tuples for diagnostic
    /// rendering.
    pub custody_drained: Vec<DrainedCustody>,
    /// Active custodies that did not respond to release within the
    /// custody-drain sub-window, or whose release returned an error.
    /// Stage 3 (plugin unload) still runs against the warden; the
    /// abandoned entry only signals that the warden did not release
    /// cleanly.
    pub custody_abandoned: Vec<DrainedCustody>,
    /// Wall-clock duration of the entire shutdown pass.
    pub elapsed: Duration,
}

/// Diagnostic record of one custody encountered during stage 2 drain.
///
/// Carries enough metadata to log the (plugin, handle, shelf) triple
/// without re-querying the ledger after release.
#[derive(Debug, Clone)]
pub struct DrainedCustody {
    /// Canonical name of the warden plugin that held the custody.
    pub plugin: String,
    /// Warden-chosen handle id identifying the custody within the
    /// plugin.
    pub handle_id: String,
    /// Fully-qualified shelf the warden occupies, if known. May be
    /// `None` if the ledger only saw a state report and never the
    /// `record_custody` call (see [`crate::custody`] for the race).
    pub shelf: Option<String>,
}

/// Stage 2: drain every active custody recorded in the ledger.
///
/// Walks [`CustodyLedger::list_active`] and, for each entry that has
/// a known shelf, calls
/// [`PluginRouter::release_custody`](crate::router::PluginRouter::release_custody)
/// inside a [`tokio::time::timeout`] guard set to `window`. Custodies
/// that release within the window are reported under
/// `custody_drained`; the rest under `custody_abandoned`.
///
/// Custodies whose ledger record carries no shelf (the partial-record
/// race) are reported as abandoned with a `None` shelf, since there
/// is no warden the steward can dispatch `release_custody` against.
async fn drain_active_custodies(
    router: &Arc<PluginRouter>,
    window: Duration,
) -> (Vec<DrainedCustody>, Vec<DrainedCustody>) {
    let ledger = Arc::clone(&router.state().custody);
    let active = ledger.list_active();

    if active.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut drained = Vec::with_capacity(active.len());
    let mut abandoned = Vec::with_capacity(active.len());

    use tokio::task::JoinSet;
    let mut set: JoinSet<(String, String, String, Result<(), StewardError>)> =
        JoinSet::new();
    let mut shelfless: Vec<DrainedCustody> = Vec::new();
    for rec in active {
        match rec.shelf.clone() {
            Some(shelf) => {
                let plugin = rec.plugin.clone();
                let handle_id = rec.handle_id.clone();
                let router = Arc::clone(router);
                set.spawn(async move {
                    let handle = evo_plugin_sdk::contract::CustodyHandle::new(
                        handle_id.clone(),
                    );
                    let release_result =
                        router.release_custody(&shelf, handle).await;
                    (plugin, handle_id, shelf, release_result)
                });
            }
            None => {
                // Custodies whose ledger record had no shelf cannot
                // be dispatched against; surface them as abandoned
                // for visibility and clear the ledger row so the
                // dead reference does not survive into the next
                // boot's rehydration.
                clear_abandoned_ledger_row(
                    &ledger,
                    &rec.plugin,
                    &rec.handle_id,
                    "no shelf recorded; warden dispatch impossible",
                )
                .await;
                shelfless.push(DrainedCustody {
                    plugin: rec.plugin,
                    handle_id: rec.handle_id,
                    shelf: None,
                });
            }
        }
    }

    let collect = async {
        let mut out: Vec<(String, String, String, Result<(), StewardError>)> =
            Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(t) => out.push(t),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "custody release task panicked or was cancelled"
                    );
                }
            }
        }
        out
    };

    let outcomes = match tokio::time::timeout(window, collect).await {
        Ok(outcomes) => outcomes,
        Err(_) => {
            // Window elapsed: abort whatever is still running and
            // re-list the ledger to capture what was not drained.
            // Clear each remaining row so the post-condition of
            // drain ("ledger empty at end of shutdown") holds even
            // when the window expired — otherwise the rows survive
            // into next boot's rehydration and the cycle repeats.
            set.abort_all();
            while (set.join_next().await).is_some() {}
            for rec in ledger.list_active() {
                clear_abandoned_ledger_row(
                    &ledger,
                    &rec.plugin,
                    &rec.handle_id,
                    "drain window elapsed before release returned",
                )
                .await;
                abandoned.push(DrainedCustody {
                    plugin: rec.plugin,
                    handle_id: rec.handle_id,
                    shelf: rec.shelf,
                });
            }
            abandoned.extend(shelfless);
            return (drained, abandoned);
        }
    };

    for (plugin, handle_id, shelf, result) in outcomes {
        match result {
            Ok(()) => drained.push(DrainedCustody {
                plugin,
                handle_id,
                shelf: Some(shelf),
            }),
            Err(e) => {
                tracing::warn!(
                    plugin = %plugin,
                    handle_id = %handle_id,
                    shelf = %shelf,
                    error = %e,
                    "custody release during shutdown did not complete \
                     cleanly; clearing ledger entry to prevent phantom \
                     rehydration on next boot"
                );
                clear_abandoned_ledger_row(
                    &ledger,
                    &plugin,
                    &handle_id,
                    "warden release returned error during drain",
                )
                .await;
                abandoned.push(DrainedCustody {
                    plugin,
                    handle_id,
                    shelf: Some(shelf),
                });
            }
        }
    }

    abandoned.extend(shelfless);

    (drained, abandoned)
}

/// Remove an abandoned custody record from the ledger (and from the
/// underlying persistence store) so the row does not survive into
/// the next steward boot's rehydration.
///
/// Custody persistence is intentional for surviving a crash mid-claim,
/// but rows that the drain phase could not release are dead state —
/// the plugin process that held the custody has died with the steward
/// and the fresh plugin instance has no knowledge of the handle.
/// Leaving the row in persistence creates a perpetual phantom: every
/// subsequent boot rehydrates the row, drain calls release against a
/// plugin that disowns the handle, the release errors, the row stays.
/// This helper closes the loop by removing the row regardless of the
/// release outcome — the abandoned-list entry preserves the audit
/// trail without the persistence residue.
async fn clear_abandoned_ledger_row(
    ledger: &Arc<CustodyLedger>,
    plugin: &str,
    handle_id: &str,
    reason: &str,
) {
    if let Err(e) = ledger.release_custody(plugin, handle_id).await {
        // LOGGING.md §2: warn (recoverable anomaly — abandoned
        // custody row will rehydrate on next boot; operator should
        // review the ledger backend health).
        tracing::warn!(
            plugin = %plugin,
            handle_id = %handle_id,
            reason = %reason,
            error = %e,
            "could not clear abandoned custody ledger row; row may \
             rehydrate on next boot"
        );
    }
}

/// Stage 3 + stage 4: spawn one unload task per plugin, race them
/// against `global_deadline`, then SIGKILL any holdouts.
///
/// Each task runs [`unload_one_plugin`] on its assigned entry.
/// Tasks complete independently; a single
/// [`tokio::time::sleep`] arm in the supervising select signals the
/// deadline. When the deadline arm fires, every still-running task
/// is aborted, and the entry's child process (if any) is killed and
/// reaped by the supervising task.
///
/// Returns `(unloaded_cleanly, killed_after_deadline)`.
async fn parallel_unload_with_deadline(
    entries: Vec<Arc<PluginEntry>>,
    global_deadline: Duration,
) -> (Vec<String>, Vec<String>) {
    use tokio::task::JoinSet;

    if entries.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Keep a name -> entry-clone map outside the spawned tasks so
    // stage 4 can still reach the child slot for SIGKILL after a
    // task abort. The task moves its own clone of the entry; the
    // map holds an additional clone for the supervisor.
    let mut entry_by_name: std::collections::HashMap<String, Arc<PluginEntry>> =
        std::collections::HashMap::with_capacity(entries.len());

    let mut set: JoinSet<String> = JoinSet::new();
    for entry in entries {
        let name = entry.name.clone();
        entry_by_name.insert(name.clone(), Arc::clone(&entry));
        set.spawn(async move {
            // Errors are logged inside unload_one_plugin; we only
            // care about whether the call returns at all (clean) or
            // is aborted by the deadline.
            let _ = unload_one_plugin(entry).await;
            name
        });
    }

    let mut unloaded = Vec::new();
    let deadline = tokio::time::sleep(global_deadline);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => {
                break;
            }
            joined = set.join_next() => {
                match joined {
                    Some(Ok(name)) => {
                        unloaded.push(name);
                    }
                    Some(Err(e)) => {
                        tracing::error!(
                            error = %e,
                            "plugin unload task panicked or was cancelled"
                        );
                    }
                    None => {
                        // All tasks complete; nothing left to wait on.
                        return (unloaded, Vec::new());
                    }
                }
            }
        }
    }

    // Deadline fired with tasks still running. Abort them all and
    // SIGKILL any out-of-process children that are still alive.
    set.abort_all();

    let mut killed: Vec<String> = Vec::new();
    let cleaned: std::collections::HashSet<String> =
        unloaded.iter().cloned().collect();
    for (name, entry) in entry_by_name {
        if cleaned.contains(&name) {
            continue;
        }
        kill_holdout_child(&name, &entry).await;
        killed.push(name);
    }

    // Drain any remaining JoinSet results so we don't leak the set's
    // bookkeeping. Errors here are expected (aborts).
    while let Some(_res) = set.join_next().await {}

    (unloaded, killed)
}

/// Stage 4 of shutdown drain: walk every departing plugin's
/// claimed addressings in the subject registry and retract each
/// on the plugin's behalf.
///
/// Runs AFTER wire-unload (Stage 3) so a graceful unload that
/// successfully retracted claims via the plugin's own
/// `unload()` path is observed first; whatever survived gets
/// swept here. The common case under
/// `KillMode=control-group` (systemd's default for service
/// units) is that plugin processes share the steward's cgroup
/// and receive SIGTERM at the same instant — they exit before
/// the steward's wire-unload reaches them, so their claimed
/// addressings sit unretracted in the registry. This sweep
/// closes that gap.
///
/// Each retract goes through a freshly-constructed
/// [`RegistrySubjectAnnouncer`] tagged with the departing
/// plugin's name so the existing retract path's full
/// machinery — happenings emission, durable persistence
/// mirror, conflict-index updates — fires consistently. The
/// alternative (calling [`SubjectRegistry::retract`] directly)
/// would skip the cascade and leave the bus / persistence
/// layer in a half-state.
///
/// Errors per addressing are logged at warn level (the most
/// common error is "did not claim", expected when a plugin
/// race-retracted via its wire-unload path before the sweep
/// got there) and counted; the sweep does not abort on the
/// first error. Returns the count of successfully retracted
/// addressings.
///
/// Failure mode tolerance: if no plugin claimed any addressing
/// the sweep is a no-op (early return; no log line). The
/// addressings query is a single registry lock acquisition
/// per plugin, bounded by O(subjects × addressings_per_subject).
async fn drain_plugin_subject_claims(
    state: &Arc<crate::state::StewardState>,
    plugin_names: &[String],
) -> usize {
    let mut total_retracted = 0usize;
    for plugin in plugin_names {
        let claimed = state.subjects.addressings_claimed_by(plugin);
        if claimed.is_empty() {
            tracing::debug!(
                plugin = %plugin,
                "drain: plugin has no surviving addressing claims; skipping"
            );
            continue;
        }
        let count = claimed.len();
        tracing::debug!(
            plugin = %plugin,
            count,
            "drain: sweeping plugin's addressing claims"
        );
        let announcer = crate::context::RegistrySubjectAnnouncer::new(
            Arc::clone(&state.subjects),
            Arc::clone(&state.relations),
            state.current_catalogue(),
            Arc::clone(&state.bus),
            plugin.clone(),
        )
        .with_persistence(Arc::clone(&state.persistence))
        .with_conflict_index(Arc::clone(&state.conflict_index));
        let mut retracted = 0usize;
        for (addressing, canonical_id) in &claimed {
            tracing::debug!(
                plugin = %plugin,
                scheme = %addressing.scheme,
                value = %addressing.value,
                canonical_id = %canonical_id,
                "drain: retracting addressing"
            );
            match announcer
                .retract(
                    addressing.clone(),
                    Some("steward drain: plugin departing".to_string()),
                )
                .await
            {
                Ok(()) => {
                    retracted += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %plugin,
                        scheme = %addressing.scheme,
                        value = %addressing.value,
                        error = %e,
                        "drain: addressing retract failed; \
                         continuing sweep"
                    );
                }
            }
        }
        total_retracted += retracted;
        tracing::info!(
            plugin = %plugin,
            claims_total = count,
            claims_retracted = retracted,
            "drain: plugin claim sweep complete"
        );
    }
    total_retracted
}

// Pre-admission validation lives in `admission/validation.rs`.
// Callers reference the helpers as
// `validation::check_manifest_prerequisites` and
// `validation::check_admin_trust`. Module-level rustdoc on each
// helper (including the rationale block on the prerequisites gate
// that previously lived here) moved with the implementations.

/// Build a v0 LoadContext for a plugin.
///
/// The context carries per-plugin filesystem paths, the plugin's
/// operator-supplied config (loaded from
/// `<plugins_config_dir>/<plugin_name>.toml` if present; empty table
/// otherwise — see [`load_plugin_config`]), no deadline,
/// logging-only implementations of the state and interaction
/// callbacks, and real announcers backed by the supplied registry and
/// graph. The `catalogue` Arc is handed to BOTH announcers: the
/// subject announcer consults it to refuse announcements of
/// undeclared types, and the relation announcer consults it to refuse
/// assertions naming an undeclared predicate and to refuse
/// source/target subject types that do not satisfy the predicate's
/// declared `source_type` / `target_type` constraints. The `bus` Arc
/// is handed to the relation announcer so it can emit
/// [`Happening::RelationCardinalityViolation`] after successful
/// asserts that exceed the declared cardinality bound on either side.
///
/// Returns an error if the per-plugin config file exists but is
/// malformed; admission of that plugin must not silently fall back
/// to an empty table when the operator has explicitly authored a
/// config file. Missing file is not an error.
// `build_load_context` deliberately takes one Arc per shared
// service (registry, graph, catalogue, bus, admin ledger, router)
// to keep each call site a flat list of named handles. Bundling
// them into a struct would just push the same set of clones one
// indirection deeper without removing the parameter count.
#[allow(clippy::too_many_arguments)]
fn build_load_context(
    plugin_data_root: &Path,
    plugins_config_dir: &Path,
    manifest: &Manifest,
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
    catalogue: Arc<Catalogue>,
    bus: Arc<HappeningBus>,
    admin_ledger: Arc<AdminLedger>,
    router: Arc<PluginRouter>,
    persistence: Arc<dyn PersistenceStore>,
    conflict_index: Arc<SubjectConflictIndex>,
    appointments_runtime: Option<Arc<crate::appointments::AppointmentRuntime>>,
    watches_runtime: Option<Arc<crate::watches::WatchRuntime>>,
    stream_coordinator: Option<Arc<crate::streams::StreamCoordinator>>,
    metadata_chain: Option<Arc<crate::metadata::MetadataChain>>,
    scheduler_runtime: Option<Arc<crate::scheduler::SchedulerRuntime>>,
    ledger: Option<Arc<crate::ledger::LedgerPrimitive>>,
    audio_routing_runtime: Option<
        Arc<crate::audio_routing::AudioRoutingRuntime>,
    >,
    audio_plane_runtime: Option<Arc<crate::audio_plane::AudioPlaneRuntime>>,
    group_store: Option<Arc<crate::groups::GroupStore>>,
    multiroom_substrate: Option<
        Arc<dyn evo_plugin_sdk::multiroom_substrate::MultiroomSubstrateHandle>,
    >,
    asset_cache: Option<
        Arc<dyn evo_plugin_sdk::contract::asset_cache::AssetCache>,
    >,
    shelf_request_dispatcher: Option<
        Arc<dyn evo_plugin_sdk::contract::ShelfRequestDispatcher>,
    >,
    credential_vault: Option<Arc<crate::credentials::CredentialVault>>,
    credential_change_bus: Arc<crate::credentials::CredentialChangeBus>,
    online_provider_config_store: Option<
        Arc<crate::online_providers::OnlineProviderConfigStore>,
    >,
    online_provider_config_bus: Arc<
        crate::online_providers::OnlineProviderConfigBus,
    >,
    revoked_capabilities: &std::collections::HashSet<String>,
) -> Result<LoadContext, StewardError> {
    // Every admit path (OOP discovery, in-process programmatic,
    // factory) routes through this builder, so creating the state
    // + credentials directories here closes the gap where the
    // discovery-only `ensure_plugin_state_and_credentials` helper
    // left in-process admissions with non-existent state_dir's
    // and silently-failing log writes.
    crate::plugin_discovery::ensure_plugin_state_and_credentials(
        plugin_data_root,
        &manifest.plugin.name,
    )?;
    let state_dir = plugin_data_root.join(&manifest.plugin.name).join("state");
    let credentials_dir = plugin_data_root
        .join(&manifest.plugin.name)
        .join("credentials");

    // Admin callbacks are populated only for plugins whose
    // manifest declares capabilities.admin = true AND whose
    // effective trust class passes the admin-trust gate (the gate
    // runs earlier at every admit entry point via
    // check_admin_trust, so by the time we reach here an admin
    // plugin is already known to qualify). Non-admin plugins see
    // None for both fields.
    //
    // Operator-revoked capabilities suppress the corresponding
    // LoadContext handle regardless of the manifest's per-
    // capability flag. The effective gate is
    // `manifest.capabilities.X && !revoked.contains("X")`. The
    // engine pre-fetches the revoked set at every admission entry
    // point and threads it through; an empty set is the no-op
    // (production boot wires a store unconditionally; in-process
    // test harnesses pass an empty set when they do not exercise
    // operator revocation).
    let admin_capable =
        manifest.capabilities.admin && !revoked_capabilities.contains("admin");
    let fast_path_capable = manifest.capabilities.fast_path
        && !revoked_capabilities.contains("fast_path");
    let appointments_capable = manifest.capabilities.appointments
        && !revoked_capabilities.contains("appointments");
    let streams_capable = manifest.capabilities.streams
        && !revoked_capabilities.contains("streams");
    let notifications_capable = manifest.capabilities.notifications
        && !revoked_capabilities.contains("notifications");
    let metadata_capable = manifest.capabilities.metadata
        && !revoked_capabilities.contains("metadata");
    let scheduler_capable = manifest.capabilities.scheduler
        && !revoked_capabilities.contains("scheduler");
    let watches_capable = manifest.capabilities.watches
        && !revoked_capabilities.contains("watches");

    // Both admin announcers carry an `Arc<PluginRouter>` so the
    // wiring layer can refuse `target_plugin` arguments that do
    // not name a currently-admitted plugin (typo guard) before
    // any storage-primitive call.
    let (subject_admin, relation_admin) = if admin_capable {
        let subject_admin: Arc<dyn evo_plugin_sdk::contract::SubjectAdmin> =
            Arc::new(
                RegistrySubjectAdmin::new(
                    Arc::clone(&registry),
                    Arc::clone(&graph),
                    Arc::clone(&catalogue),
                    Arc::clone(&bus),
                    Arc::clone(&admin_ledger),
                    Arc::clone(&router),
                    manifest.plugin.name.clone(),
                )
                .with_persistence(Arc::clone(&persistence))
                .with_conflict_index(Arc::clone(&conflict_index)),
            );
        let relation_admin: Arc<dyn evo_plugin_sdk::contract::RelationAdmin> =
            Arc::new(
                RegistryRelationAdmin::new(
                    Arc::clone(&registry),
                    Arc::clone(&graph),
                    Arc::clone(&catalogue),
                    Arc::clone(&bus),
                    admin_ledger,
                    Arc::clone(&router),
                    manifest.plugin.name.clone(),
                )
                .with_persistence(Arc::clone(&persistence)),
            );
        (Some(subject_admin), Some(relation_admin))
    } else {
        (None, None)
    };

    let config = load_plugin_config(plugins_config_dir, &manifest.plugin.name)?;

    // Fast Path dispatcher is populated only for plugins whose
    // manifest declares capabilities.fast_path = true. Plugins
    // that did not opt in see None and an unwrap at use site
    // surfaces the manifest misconfiguration loudly. The router-
    // backed dispatcher routes each call through the per-warden
    // Fast Path verb gate and budget enforcement.
    let fast_path_dispatcher: Option<
        Arc<dyn evo_plugin_sdk::contract::FastPathDispatcher>,
    > = if fast_path_capable {
        Some(Arc::new(crate::context::RouterFastPathDispatcher::new(
            Arc::clone(&router),
        )))
    } else {
        None
    };

    // Appointment scheduler is populated only for plugins whose
    // manifest declares capabilities.appointments = true AND the
    // engine was constructed with a runtime handle. Plugins that
    // did not opt in see None and an unwrap at use site surfaces
    // the manifest misconfiguration loudly. The router-backed
    // scheduler binds the dispatcher to the plugin's canonical
    // name so every appointment is namespaced under that creator
    // label and counted against that creator's quota.
    let appointments: Option<
        Arc<dyn evo_plugin_sdk::contract::AppointmentScheduler>,
    > = match (&appointments_runtime, appointments_capable) {
        (Some(runtime), true) => {
            Some(Arc::new(crate::context::RouterAppointmentScheduler::new(
                Arc::clone(runtime),
                manifest.plugin.name.clone(),
            )))
        }
        _ => None,
    };

    // Stream host is populated only for plugins whose manifest
    // declares capabilities.streams = true AND the engine was
    // constructed with a stream-coordinator handle. Plugins that
    // did not opt in see None and an unwrap at use site surfaces
    // the manifest misconfiguration loudly. The coordinator-
    // backed host translates the SDK trait's id-only producer
    // contract onto the coordinator's internal StreamHandle
    // surface.
    let streams: Option<Arc<dyn evo_plugin_sdk::contract::StreamHost>> =
        match (&stream_coordinator, streams_capable) {
            (Some(coordinator), true) => {
                let mut host = crate::context::CoordinatorStreamHost::new(
                    Arc::clone(coordinator),
                    manifest.plugin.name.clone(),
                );
                if let Some(ledger) = &ledger {
                    host = host
                        .with_telemetry(Arc::clone(&bus), Arc::clone(ledger));
                }
                Some(Arc::new(host))
            }
            _ => None,
        };

    // Notifications adapter. The dispatcher state lives in the
    // reference plugin (`org.evoframework.system.notifications`);
    // the framework only forwards trait calls to the plugin's
    // `system.notifications.send` / `system.notifications.cancel`
    // verbs via the shelf-request-dispatcher. Populated only for
    // plugins whose manifest declares `capabilities.notifications
    // = true` AND when the engine was configured with a
    // shelf-request-dispatcher (production boot always is).
    // Non-declaring plugins see `None` on the load context; calls
    // panic on unwrap — the intended fail-fast for a manifest
    // authoring mistake.
    let notifications: Option<
        Arc<dyn evo_plugin_sdk::contract::NotificationEmitter>,
    > = match (&shelf_request_dispatcher, notifications_capable) {
        (Some(dispatcher), true) => Some(Arc::new(
            crate::context::VerbDispatchNotificationEmitter::new(
                Arc::clone(dispatcher),
                manifest.plugin.name.clone(),
            ),
        )),
        _ => None,
    };

    // Metadata consumer is populated only for plugins whose
    // manifest declares capabilities.metadata = true AND the
    // engine was constructed with a metadata-chain handle. The
    // chain-backed consumer translates ChainError to MetadataError
    // at the boundary so plugins see a single error type across
    // the producer / consumer surfaces.
    let metadata: Option<Arc<dyn evo_plugin_sdk::contract::MetadataConsumer>> =
        match (&metadata_chain, metadata_capable) {
            (Some(chain), true) => {
                let mut consumer = crate::context::ChainMetadataConsumer::new(
                    Arc::clone(chain),
                    manifest.plugin.name.clone(),
                );
                if let Some(ledger) = &ledger {
                    consumer = consumer
                        .with_telemetry(Arc::clone(&bus), Arc::clone(ledger));
                }
                Some(Arc::new(consumer))
            }
            _ => None,
        };

    // Scheduler is populated only for plugins whose manifest
    // declares capabilities.scheduler = true AND the engine was
    // constructed with a scheduler runtime handle. Plugins that
    // did not opt in see None and an unwrap at use site surfaces
    // the manifest misconfiguration loudly. The router-backed
    // scheduler binds the dispatcher to the plugin's canonical
    // name so every task is namespaced under that creator label.
    let scheduler: Option<Arc<dyn evo_plugin_sdk::contract::Scheduler>> =
        match (&scheduler_runtime, scheduler_capable) {
            (Some(runtime), true) => {
                Some(Arc::new(crate::context::RouterScheduler::new(
                    Arc::clone(runtime),
                    manifest.plugin.name.clone(),
                )))
            }
            _ => None,
        };

    // Audio routing handle is populated only for plugins
    // whose manifest declares an audio capability — source
    // with an audio output_kind, delivery, or composition —
    // AND the engine was constructed with an audio_routing
    // runtime. The runtime broker hands a per-plugin handle;
    // the plugin uses it to fetch the OS-native endpoint the
    // framework configured for its chain stage. Audio bytes
    // do NOT traverse this handle — it returns endpoint
    // identifiers (path / port / shm region) the plugin opens
    // directly.
    let audio_capable = audio_routing_role(manifest).is_some();
    let audio_routing: Option<
        Arc<dyn evo_plugin_sdk::contract::audio_routing::AudioRouting>,
    > = match (&audio_routing_runtime, audio_capable) {
        (Some(runtime), true) => Some(runtime.handle_for_plugin(
            &manifest.plugin.name,
            audio_routing_role(manifest).expect("checked above"),
        )),
        _ => None,
    };

    // Per-plugin scoped user-interaction requester. Built here
    // once so both LoadContext.user_interaction_requester and the
    // credential-vault handle's prompt-on-missing helper reference
    // the same Arc (single dispatch surface).
    let user_interaction_requester: Arc<
        dyn evo_plugin_sdk::UserInteractionRequester,
    > = Arc::new(LoggingUserInteractionRequester::new(
        manifest.plugin.name.clone(),
    ));

    // Per-plugin scoped credential-vault handle. The engine's
    // `credential_vault` slot is optional at construction (a
    // steward built without wiring it leaves the slot empty and
    // plugins fall back to their pre-substrate credential paths);
    // when populated, every admission produces a handle bound to
    // this plugin's canonical id so the plugin cannot address
    // another plugin's rows.
    let credential_vault_handle: Option<
        Arc<dyn evo_plugin_sdk::contract::context::CredentialVaultHandle>,
    > = credential_vault.as_ref().map(|vault| {
        // Look up (or lazily register) this plugin's sender on the
        // central credential-change bus. Framework wire-op handlers
        // publish on the same sender after a successful
        // `credential_put` / `credential_delete`, and every active
        // `subscribe_changes` receiver on the returned handle
        // observes the mutation. Idempotent — admission that happens
        // before or after the first publish gets the same sender.
        let change_tx = credential_change_bus.sender_for(&manifest.plugin.name);
        let handle: Arc<
            dyn evo_plugin_sdk::contract::context::CredentialVaultHandle,
        > = Arc::new(crate::context::PluginScopedCredentialVault::new(
            Arc::clone(vault),
            manifest.plugin.name.clone(),
            Arc::clone(&user_interaction_requester),
            manifest.plugin.name.clone(),
            change_tx,
        ));
        handle
    });

    Ok(LoadContext {
        config,
        state_dir,
        credentials_dir,
        deadline: None,
        state_reporter: Arc::new(LoggingStateReporter::new(
            manifest.plugin.name.clone(),
        )),
        instance_announcer: Arc::new(LoggingInstanceAnnouncer::new(
            manifest.plugin.name.clone(),
        )),
        user_interaction_requester: Arc::clone(&user_interaction_requester),
        happening_emitter: Arc::new(
            crate::context::RouterHappeningEmitter::new(
                Arc::clone(&bus),
                manifest.plugin.name.clone(),
            ),
        ),
        subject_announcer: Arc::new(
            RegistrySubjectAnnouncer::new(
                Arc::clone(&registry),
                Arc::clone(&graph),
                Arc::clone(&catalogue),
                Arc::clone(&bus),
                manifest.plugin.name.clone(),
            )
            .with_persistence(Arc::clone(&persistence))
            .with_conflict_index(conflict_index),
        ),
        relation_announcer: Arc::new(
            RegistryRelationAnnouncer::new(
                Arc::clone(&registry),
                graph,
                catalogue,
                bus,
                manifest.plugin.name.clone(),
            )
            .with_persistence(persistence),
        ),
        // Subject querying is read-only and emits no happenings or
        // audit entries; populate the querier for every in-process
        // plugin regardless of capability or trust class. The
        // out-of-process wire-side dispatch wires its own adapter
        // separately.
        subject_querier: Some(Arc::new(RegistrySubjectQuerier::new(
            Arc::clone(&registry),
        ))),
        subject_admin,
        relation_admin,
        fast_path_dispatcher,
        appointments,
        watches: match (&watches_runtime, watches_capable) {
            (Some(runtime), true) => {
                Some(Arc::new(crate::context::RouterWatchScheduler::new(
                    Arc::clone(runtime),
                    manifest.plugin.name.clone(),
                )))
            }
            _ => None,
        },
        streams,
        notifications,
        metadata,
        scheduler,
        audio_routing,
        // Subject-state subscriber. Populated for every
        // in-process plugin from the same SubjectRegistry the
        // subject_querier reads. Read-only push channel; emits
        // no happenings; no audit-ledger entries; capability
        // gate via the manifest's `capabilities.subscribe_subjects`
        // declaration is enforced at the plugin's own load
        // path (unwrap or fail loudly when None).
        subject_state_subscriber: Some(Arc::new(
            crate::context::RegistrySubjectStateSubscriber::new(Arc::clone(
                &registry,
            )),
        )),
        // Audio-plane handle. Populated only when the engine
        // was constructed via `with_audio_plane(...)` AND the
        // plugin's manifest declares `capabilities.audio_plane
        // = true`. Manifest gate enforced here; runtime-
        // absence (test harnesses) yields None unconditionally.
        audio_plane: match (
            audio_plane_runtime.as_ref(),
            group_store.as_ref(),
            manifest.capabilities.audio_plane,
        ) {
            (Some(runtime), Some(store), true) => Some(Arc::new(
                crate::context::RuntimeAudioPlaneHandle::new(
                    Arc::clone(runtime),
                    Arc::clone(store),
                ),
            )
                as Arc<
                    dyn evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle,
                >),
            _ => None,
        },
        // Framework asset cache. Populated when the steward
        // constructed the cache at boot AND admitted this
        // plugin through an engine equipped via
        // `with_asset_cache(...)`. Plugins fall back to
        // placeholder rendering when None per the universal
        // artwork-first-or-icon rule.
        asset_cache: asset_cache.clone(),
        // Multi-room substrate consumption handle. Populated
        // when the framework wires `with_multiroom_substrate`
        // at boot (production). Reactive-only plugins
        // subscribe + reconfigure in place; absent leaves
        // them in substrate-empty mode (admit with defaults,
        // do not subscribe).
        multiroom_substrate: multiroom_substrate.clone(),
        // Shelf-request dispatcher. In-process plugins receive
        // the framework's direct-routing implementation that
        // routes through the same dispatch_request machinery
        // the wire-op layer uses. The dispatcher is constructed
        // at the steward level (it holds the router + state
        // handles) and shared via Arc; admission threads the
        // shared instance into every plugin's load context.
        // None is the safe degradation: plugins gracefully fall
        // back to local-only resolution paths.
        shelf_request_dispatcher: shelf_request_dispatcher.clone(),
        credential_vault: credential_vault_handle,
        online_provider_config: online_provider_config_store
            .as_ref()
            .map(|store| {
                Arc::new(crate::online_providers::SharedOnlineProviderConfigHandle::new(
                    Arc::clone(store),
                    Arc::clone(&online_provider_config_bus),
                )) as Arc<
                    dyn evo_plugin_sdk::contract::context::OnlineProviderConfigHandle,
                >
            }),
        // Capability resolution map. The framework's
        // admission-time preflight runner is wired in
        // dedicated chunk P2.5: it loads the plugin's
        // privileges.yaml, generates probes per declared
        // intent, executes them via the SDK probe runner,
        // and populates this map. Until that lands, every
        // in-process plugin sees an empty map and falls
        // back to its own runtime detection (EUID dispatch,
        // best-effort sudoers attempt, etc.) — which is the
        // same shape volumio-evo has run successfully in
        // production. The empty-map default is intentional
        // and non-breaking.
        capabilities: Arc::new(
            evo_plugin_sdk::privileges::CapabilityResolutionMap::new(),
        ),
        // `None` here because the watch is wired in
        // `invoke_plugin_load` AFTER the initial probe run —
        // the framework needs the snapshot of plans (returned
        // from `handle.probe_plans()`) to spawn the re-probe
        // task. `build_load_context` runs before any plugin
        // handle is bound, so it cannot construct the watch.
        capabilities_watch: None,
    })
}

/// Determine the plugin's role in the audio chain from its
/// manifest. Returns `Some` when the plugin declares an audio
/// capability — `[capabilities.delivery]` (Delivery role),
/// `[capabilities.composition]` (Composition role), or
/// `[capabilities.source]` with an audio `output_kind`
/// (Source role) — `None` otherwise.
///
/// Used by [`build_load_context`] to gate the
/// [`crate::audio_routing::RouterAudioRouting`] LoadContext
/// stamping.
fn audio_routing_role(
    manifest: &Manifest,
) -> Option<crate::audio_routing::PluginAudioRole> {
    if manifest.capabilities.delivery.is_some() {
        return Some(crate::audio_routing::PluginAudioRole::Delivery);
    }
    if manifest.capabilities.composition.is_some() {
        return Some(crate::audio_routing::PluginAudioRole::Composition);
    }
    if let Some(source) = &manifest.capabilities.source {
        if let Some(kind) = &source.output_kind {
            if matches!(kind.as_str(), "audio.pcm" | "audio.encoded") {
                return Some(crate::audio_routing::PluginAudioRole::Source);
            }
        }
    }
    None
}

/// Load `<plugins_config_dir>/<plugin_name>.toml` into a
/// [`toml::Table`].
///
/// Operator-facing surface: the documented contract on
/// [`evo_plugin_sdk::contract::LoadContext::config`] is that the
/// per-plugin operator config file (default
/// `/etc/evo/plugins.d/<name>.toml`) is merged into the plugin's
/// load-time configuration. A missing file is not an error: an
/// operator who has not authored a config for this plugin sees an
/// empty table. A present-but-malformed file IS an error: silently
/// admitting the plugin with an empty table on a typo would mask the
/// operator's intent and produce confusing runtime behaviour.
///
/// The error path returns [`StewardError::Manifest`] (the
/// manifest-shaped variant — operator-supplied configuration is in
/// the same admissibility class as a malformed manifest); the
/// admission entry points propagate it so the operator sees the
/// failure at admission time, not at the plugin's first
/// config-driven request.
fn load_plugin_config(
    plugins_config_dir: &Path,
    plugin_name: &str,
) -> Result<toml::Table, StewardError> {
    let path = plugins_config_dir.join(format!("{plugin_name}.toml"));
    let bytes = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(toml::Table::new());
        }
        Err(e) => {
            return Err(StewardError::io(
                format!("reading plugin config {}", path.display()),
                e,
            ));
        }
    };
    bytes.parse::<toml::Table>().map_err(|e| {
        StewardError::Admission(format!(
            "{plugin_name}: malformed plugin config {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod load_plugin_config_tests {
    use super::*;

    #[test]
    fn missing_file_returns_empty_table() {
        // The operator has not authored a config for this plugin —
        // the result is an empty `toml::Table`, not an error. This
        // is the documented contract on `LoadContext.config`.
        let dir = tempfile::tempdir().unwrap();
        let table = load_plugin_config(dir.path(), "org.test.no.config")
            .expect("missing file is not an error");
        assert!(table.is_empty(), "expected empty table, got {table:?}");
    }

    #[test]
    fn well_formed_file_loads_into_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("org.test.has.config.toml");
        std::fs::write(
            &path,
            "[ui]\n\
             theme = \"dark\"\n\
             refresh_ms = 250\n\
             [paths]\n\
             cache_dir = \"/var/cache/test\"\n",
        )
        .unwrap();

        let table = load_plugin_config(dir.path(), "org.test.has.config")
            .expect("well-formed config must load");

        let ui = table.get("ui").and_then(|v| v.as_table()).unwrap();
        assert_eq!(ui.get("theme").and_then(|v| v.as_str()), Some("dark"));
        assert_eq!(
            ui.get("refresh_ms").and_then(|v| v.as_integer()),
            Some(250)
        );

        let paths = table.get("paths").and_then(|v| v.as_table()).unwrap();
        assert_eq!(
            paths.get("cache_dir").and_then(|v| v.as_str()),
            Some("/var/cache/test"),
        );
    }

    #[test]
    fn malformed_file_aborts_admission_with_named_path() {
        // A present-but-malformed config MUST refuse the load — a
        // silent fall-through to an empty table would mask the
        // operator's typo and produce confusing runtime behaviour.
        // The error message MUST name both the plugin and the file
        // path so the operator can find the typo without grepping.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("org.test.malformed.toml");
        // Unclosed string — TOML parser refuses.
        std::fs::write(&path, "key = \"unterminated").unwrap();

        let err = load_plugin_config(dir.path(), "org.test.malformed")
            .expect_err("malformed config must abort");
        let msg = format!("{err}");
        assert!(
            msg.contains("org.test.malformed"),
            "error must name the plugin: {msg}"
        );
        assert!(
            msg.contains("malformed plugin config"),
            "error must explain the failure mode: {msg}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{
        Assignment, BuildInfo, CourseCorrection, CustodyHandle, Plugin,
        PluginDescription, PluginError, PluginIdentity, Request, Response,
        RuntimeCapabilities,
    };
    use std::future::Future;

    /// Minimal test respondent: passes its own identity back as the
    /// response payload.
    #[derive(Default)]
    struct TestRespondent {
        name: String,
        loaded: bool,
        /// When set, overrides the default `["ping"]` runtime
        /// request-types advertised by `describe()`. Used by URI-
        /// scheme tests that admit a source-shaped respondent
        /// declaring play-control verbs.
        runtime_request_types: Option<Vec<String>>,
    }

    impl Plugin for TestRespondent {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                let request_types = self
                    .runtime_request_types
                    .clone()
                    .unwrap_or_else(|| vec!["ping".into()]);
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types,
                        course_correct_verbs: vec![],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
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
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move {
                self.loaded = true;
                Ok(())
            }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move {
                self.loaded = false;
                Ok(())
            }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Respondent for TestRespondent {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move { Ok(Response::for_request(req, req.payload.clone())) }
        }
    }

    fn test_catalogue() -> Arc<Catalogue> {
        Arc::new(
            Catalogue::from_toml(
                r#"
schema_version = 1

[[racks]]
name = "test"
family = "domain"
charter = "test rack"

[[racks.shelves]]
name = "ping"
shape = 1
description = "test shelf"

[[racks.shelves]]
name = "custody"
shape = 1
description = "test custody shelf for warden tests"

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
"#,
            )
            .unwrap(),
        )
    }

    /// Construct an `AdmissionEngine` over a fresh `StewardState`
    /// populated with the supplied catalogue and default-constructed
    /// stores. The default plugin data root and security policy
    /// match the engine's old `new()` defaults so existing test
    /// behaviour is preserved.
    /// Process-static plugin data root for unit tests. A fresh
    /// tempdir owned by a `OnceLock` so every test in the binary
    /// shares a writable root without races on creation. The
    /// production default `/var/lib/evo/plugins` is unwritable by
    /// the test runner; pointing `AdmissionEngine` at this writable
    /// root lets `build_load_context`'s state + credentials dir
    /// creation succeed without poking at the production path.
    /// Tests that need isolation between plugin names can either
    /// use unique plugin names (the common case) or build their
    /// own engine directly with a per-test tempdir.
    fn test_plugin_data_root() -> PathBuf {
        use std::sync::OnceLock;
        static ROOT: OnceLock<PathBuf> = OnceLock::new();
        ROOT.get_or_init(|| {
            // The TempDir is intentionally leaked: it lives for the
            // process. Tests don't enumerate the dir themselves;
            // they admit plugins by unique name.
            let dir = tempfile::tempdir()
                .expect("test_plugin_data_root: cannot create tempdir");
            let p = dir.path().to_path_buf();
            std::mem::forget(dir);
            p
        })
        .clone()
    }

    fn test_engine_from_catalogue(
        catalogue: Arc<Catalogue>,
    ) -> AdmissionEngine {
        let state = StewardState::for_tests_with_catalogue(catalogue);
        AdmissionEngine::new(
            state,
            test_plugin_data_root(),
            std::path::PathBuf::new(),
            None,
            PluginsSecurityConfig::default(),
        )
    }

    /// Construct an `AdmissionEngine` over the standard test
    /// catalogue (`test_catalogue`) and a fresh state bag. The
    /// catalogue is the most common pre-condition for engine tests
    /// in this module.
    fn test_engine() -> AdmissionEngine {
        test_engine_from_catalogue(test_catalogue())
    }

    /// Construct an `AdmissionEngine` over the supplied
    /// `StewardState`. Used by tests that need to assert on shared
    /// store handles outside the engine.
    fn engine_with_state(state: Arc<StewardState>) -> AdmissionEngine {
        AdmissionEngine::new(
            state,
            test_plugin_data_root(),
            std::path::PathBuf::new(),
            None,
            PluginsSecurityConfig::default(),
        )
    }

    /// Construct an `AdmissionEngine` over the supplied catalogue and
    /// caller-provided plugin trust state. Used by trust-gating tests.
    fn test_engine_with_trust(
        catalogue: Arc<Catalogue>,
        trust: Arc<crate::plugin_trust::PluginTrustState>,
    ) -> AdmissionEngine {
        let state = StewardState::for_tests_with_catalogue(catalogue);
        AdmissionEngine::new(
            state,
            test_plugin_data_root(),
            std::path::PathBuf::new(),
            Some(trust),
            PluginsSecurityConfig::default(),
        )
    }

    fn test_manifest(name: &str) -> Manifest {
        let toml = format!(
            r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "test.ping"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
mode = "reload-cleanable"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 1000
"#
        );
        Manifest::from_toml(&toml).unwrap()
    }

    #[tokio::test]
    async fn admits_valid_plugin() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let manifest = test_manifest("org.test.ping");
        engine
            .admit_singleton_respondent(plugin, manifest)
            .await
            .unwrap();
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn reload_plugin_refuses_unknown_name() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let _runtime_dir = tempfile::tempdir().unwrap();
        let r = engine.reload_plugin("org.test.never-admitted").await;
        match r {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("no plugin admitted with name"),
                    "expected 'no plugin admitted with name' refusal, got: {msg}"
                );
            }
            other => panic!("expected Dispatch refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reload_plugin_refuses_programmatic_origin() {
        // A plugin admitted via the typed admit_singleton_respondent
        // path has no recorded source directory. With Restart mode
        // (test_manifest's default), reload must refuse with a
        // structured error pointing the distribution at unload +
        // admit, not silently succeed with "nothing to do" or
        // panic on the missing origin.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let _runtime_dir = tempfile::tempdir().unwrap();
        let r = engine.reload_plugin("org.test.ping").await;
        match r {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("admitted programmatically"),
                    "expected 'admitted programmatically' refusal, got: {msg}"
                );
            }
            other => panic!("expected Dispatch refusal, got {other:?}"),
        }
    }

    /// Build a copy of [`test_manifest`] with the `[lifecycle] mode`
    /// field overridden to the requested value. Used by the
    /// `reload_plugin` per-mode dispatch tests to construct fixtures
    /// covering the Frozen and ReactiveOnly branches.
    fn test_manifest_with_mode(
        name: &str,
        mode: evo_plugin_sdk::manifest::LifecycleMode,
    ) -> Manifest {
        let mut m = test_manifest(name);
        m.require_lifecycle_mut().mode = mode;
        m
    }

    #[tokio::test]
    async fn reload_plugin_refuses_frozen_mode() {
        // Frozen-mode plugins decline runtime reload entirely: a
        // wire-op gesture or file-watcher edit dispatches but the
        // engine returns a structured refusal so the operator can
        // see why no reload happened.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(
                plugin,
                test_manifest_with_mode(
                    "org.test.ping",
                    evo_plugin_sdk::manifest::LifecycleMode::Frozen,
                ),
            )
            .await
            .unwrap();

        let r = engine.reload_plugin("org.test.ping").await;
        match r {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("mode = frozen"),
                    "expected 'mode = frozen' refusal, got: {msg}"
                );
            }
            other => panic!("expected Dispatch refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reload_plugin_acknowledges_reactive_only_mode() {
        // Reactive-only plugins acknowledge the reload request
        // without teardown — their operator state lives in
        // framework substrates and is updated via wire ops, not
        // TOML diffs. The dispatch returns Ok(()).
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(
                plugin,
                test_manifest_with_mode(
                    "org.test.ping",
                    evo_plugin_sdk::manifest::LifecycleMode::ReactiveOnly,
                ),
            )
            .await
            .unwrap();

        engine
            .reload_plugin("org.test.ping")
            .await
            .expect("reactive-only reload returns Ok(())");
    }

    #[tokio::test]
    async fn reload_manifest_swaps_enforcement_policy_atomically() {
        // Operator updates the manifest's request_types: declares
        // a new verb on top of "ping". After reload_manifest, the
        // entry's enforcement policy carries the new declarations
        // and the dispatch gate accepts the new verb.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        // Sanity-check the old policy.
        let entry = engine
            .router()
            .lookup_by_name("org.test.ping")
            .expect("admitted");
        let old_policy = entry.load_policy();
        assert_eq!(
            old_policy.allowed_request_types.as_deref(),
            Some(&["ping".to_string()][..])
        );

        // Build new manifest TOML with an extended request_types
        // list. Drift check against TestRespondent's runtime
        // describe() (which advertises only "ping") would refuse a
        // *narrower* manifest than runtime, so we keep the runtime
        // list as a subset of the manifest list — drift detection
        // permits manifest declarations to exceed runtime
        // capabilities (the inverse direction is the failure mode).
        let new_toml = r#"
[plugin]
name = "org.test.ping"
version = "0.2.0"
contract = 1

[target]
shelf = "test.ping"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 2500
"#;
        let outcome = engine
            .reload_manifest(
                "org.test.ping",
                ManifestSource::Inline(new_toml.to_string()),
                false,
            )
            .await
            .expect("reload should succeed");
        assert_eq!(outcome.from_manifest_version, "0.1.0");
        assert_eq!(outcome.to_manifest_version, "0.2.0");
        assert!(!outcome.dry_run);

        // Policy snapshot now carries the new budget.
        let new_policy = entry.load_policy();
        assert_eq!(new_policy.default_request_deadline_ms, Some(2500));
        // Cached manifest also reflects the new version.
        assert_eq!(
            entry
                .current_manifest()
                .expect("manifest cached")
                .plugin
                .version
                .to_string(),
            "0.2.0"
        );
    }

    #[tokio::test]
    async fn reload_manifest_dry_run_does_not_mutate_state() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();
        let entry = engine
            .router()
            .lookup_by_name("org.test.ping")
            .expect("admitted");
        let policy_before = entry.load_policy();
        let original_budget = policy_before.default_request_deadline_ms;

        let new_toml = r#"
[plugin]
name = "org.test.ping"
version = "0.5.0"
contract = 1

[target]
shelf = "test.ping"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 9999
"#;
        let outcome = engine
            .reload_manifest(
                "org.test.ping",
                ManifestSource::Inline(new_toml.to_string()),
                true,
            )
            .await
            .expect("dry_run should succeed");
        assert!(outcome.dry_run);
        assert_eq!(outcome.to_manifest_version, "0.5.0");

        let policy_after = entry.load_policy();
        assert_eq!(
            policy_after.default_request_deadline_ms, original_budget,
            "dry-run must not mutate the enforcement policy"
        );
        let manifest_after = entry.current_manifest().expect("manifest cached");
        assert_eq!(
            manifest_after.plugin.version.to_string(),
            "0.1.0",
            "dry-run must not swap the cached manifest"
        );
    }

    #[tokio::test]
    async fn reload_manifest_refuses_identity_mismatch() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        // Same TOML but with a different plugin.name field.
        let mismatched = r#"
[plugin]
name = "org.test.different"
version = "0.1.0"
contract = 1

[target]
shelf = "test.ping"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 1000
"#;
        let r = engine
            .reload_manifest(
                "org.test.ping",
                ManifestSource::Inline(mismatched.to_string()),
                false,
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("declares plugin name"),
                    "expected identity-mismatch refusal, got: {msg}"
                );
            }
            other => panic!("expected Admission refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reload_manifest_refuses_transport_change() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        // Same plugin name + version but transport switched to OOP.
        let oop_toml = r#"
[plugin]
name = "org.test.ping"
version = "0.1.0"
contract = 1

[target]
shelf = "test.ping"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "/usr/local/bin/ping-plugin"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 1000
"#;
        let r = engine
            .reload_manifest(
                "org.test.ping",
                ManifestSource::Inline(oop_toml.to_string()),
                false,
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("transport.kind"),
                    "expected transport-change refusal, got: {msg}"
                );
            }
            other => panic!("expected Admission refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reload_catalogue_swaps_declarations_atomically() {
        // Build a starting catalogue with one shelf that
        // test_manifest plugins admit against, then reload to a
        // catalogue that adds a second shelf. The swap should
        // succeed; admitted plugins keep their entries; the new
        // catalogue is observable via state.current_catalogue().
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let extended_toml = r#"
schema_version = 1

[[racks]]
name = "test"
family = "domain"
charter = "test rack"

[[racks.shelves]]
name = "ping"
shape = 1

[[racks.shelves]]
name = "extended"
shape = 1
"#;
        let outcome = engine
            .reload_catalogue(
                ManifestSource::Inline(extended_toml.to_string()),
                false,
            )
            .await
            .expect("reload should succeed");
        assert_eq!(outcome.rack_count, 1);
        assert!(!outcome.dry_run);

        let after = engine.state().current_catalogue();
        assert!(after.find_shelf("test.extended").is_some());
        assert!(after.find_shelf("test.ping").is_some());
    }

    #[tokio::test]
    async fn reload_catalogue_dry_run_does_not_mutate() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let extended_toml = r#"
schema_version = 1

[[racks]]
name = "test"
family = "domain"
charter = "test rack"

[[racks.shelves]]
name = "ping"
shape = 1

[[racks.shelves]]
name = "added-by-dry-run"
shape = 1
"#;
        let outcome = engine
            .reload_catalogue(
                ManifestSource::Inline(extended_toml.to_string()),
                true,
            )
            .await
            .expect("dry_run should succeed");
        assert!(outcome.dry_run);

        // Catalogue must not have changed.
        let after = engine.state().current_catalogue();
        assert!(
            after.find_shelf("test.added-by-dry-run").is_none(),
            "dry-run must not swap the catalogue"
        );
    }

    #[tokio::test]
    async fn reload_catalogue_refuses_when_running_plugin_shelf_removed() {
        // Admit a plugin against test.ping, then try to reload to a
        // catalogue that no longer declares test.ping. The reload
        // must refuse with a structured error and emit a
        // CardinalityViolation per orphaned shelf; the catalogue
        // stays unchanged.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let removed_toml = r#"
schema_version = 1

[[racks]]
name = "test"
family = "domain"
charter = "test rack"

[[racks.shelves]]
name = "different-shelf"
shape = 1
"#;
        let r = engine
            .reload_catalogue(
                ManifestSource::Inline(removed_toml.to_string()),
                false,
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("shelf-occupancy conflict"),
                    "expected shelf-occupancy refusal, got: {msg}"
                );
            }
            other => panic!("expected Admission refusal, got {other:?}"),
        }
        // Catalogue unchanged.
        let after = engine.state().current_catalogue();
        assert!(after.find_shelf("test.ping").is_some());
    }

    #[tokio::test]
    async fn reload_catalogue_refuses_invalid_toml() {
        let catalogue = test_catalogue();
        let engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let bogus = "this is not valid toml :::";
        let r = engine
            .reload_catalogue(ManifestSource::Inline(bogus.to_string()), false)
            .await;
        assert!(r.is_err(), "parse failure must surface as an error");
    }

    #[tokio::test]
    async fn enable_plugin_persists_enabled_bit() {
        let catalogue = test_catalogue();
        let engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let outcome = engine
            .enable_plugin("org.test.unknown", Some("operator note".into()))
            .await
            .expect("enable should succeed");
        assert!(!outcome.was_currently_admitted);
        let rows = engine
            .persistence()
            .load_all_installed_plugins()
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].enabled);
        assert_eq!(rows[0].last_state_reason.as_deref(), Some("operator note"));
    }

    #[tokio::test]
    async fn disable_plugin_drains_admitted_and_persists_disabled_bit() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();
        assert_eq!(engine.len(), 1);

        let outcome = engine
            .disable_plugin("org.test.ping", Some("disabling".into()))
            .await
            .expect("disable should succeed");
        assert!(outcome.was_currently_admitted);
        // Plugin drained out of the router.
        assert_eq!(engine.len(), 0);
        // Persistence row carries enabled = false.
        let rows = engine
            .persistence()
            .load_all_installed_plugins()
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].enabled);
    }

    #[tokio::test]
    async fn disable_plugin_refuses_essential_shelf() {
        // Build a catalogue where the test.ping shelf is declared
        // required = true; admit a plugin against it; disable
        // refuses with a structured error and the plugin remains
        // admitted.
        let toml = r#"
schema_version = 1

[[racks]]
name = "test"
family = "domain"
charter = "test"

[[racks.shelves]]
name = "ping"
shape = 1
required = true
"#;
        let cat =
            Arc::new(crate::catalogue::Catalogue::from_toml(toml).unwrap());
        let mut engine = test_engine_from_catalogue(Arc::clone(&cat));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let r = engine.disable_plugin("org.test.ping", None).await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("required = true"),
                    "expected essential refusal, got: {msg}"
                );
            }
            other => panic!("expected Admission refusal, got {other:?}"),
        }
        // Plugin still admitted.
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn purge_plugin_state_wipes_state_and_credentials_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_data_root = dir.path().to_path_buf();
        let state_dir = plugin_data_root.join("org.test.ping").join("state");
        let cred_dir =
            plugin_data_root.join("org.test.ping").join("credentials");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(&cred_dir).unwrap();
        std::fs::write(state_dir.join("blob.bin"), b"old data").unwrap();
        std::fs::write(cred_dir.join("creds.toml"), b"secret").unwrap();

        let catalogue = test_catalogue();
        let state = StewardState::for_tests_with_catalogue(catalogue);
        let engine = AdmissionEngine::new(
            state,
            plugin_data_root.clone(),
            std::path::PathBuf::new(),
            None,
            PluginsSecurityConfig::default(),
        );

        let outcome = engine
            .purge_plugin_state("org.test.ping")
            .await
            .expect("purge should succeed");
        assert!(!outcome.was_currently_admitted);
        // Files gone, directories preserved (recreated empty).
        assert!(state_dir.is_dir());
        assert!(cred_dir.is_dir());
        assert!(!state_dir.join("blob.bin").exists());
        assert!(!cred_dir.join("creds.toml").exists());
    }

    #[tokio::test]
    async fn rejects_plugin_with_identity_mismatch() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            // describe() will return this name
            name: "org.test.actual".into(),
            ..Default::default()
        };
        // But manifest says a different name
        let manifest = test_manifest("org.test.claimed");
        let r = engine.admit_singleton_respondent(plugin, manifest).await;
        assert!(matches!(r, Err(StewardError::Admission(_))));
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn rejects_plugin_targeting_missing_shelf() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        // Manifest targets nonexistent shelf
        let toml = r#"
[plugin]
name = "org.test.ping"
version = "0.1.0"
contract = 1

[target]
shelf = "nonexistent.shelf"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 1000
"#;
        let manifest = Manifest::from_toml(toml).unwrap();
        let r = engine.admit_singleton_respondent(plugin, manifest).await;
        assert!(matches!(r, Err(StewardError::Admission(_))));
    }

    #[tokio::test]
    async fn rejects_duplicate_shelf_admission() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let p1 = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(p1, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let p2 = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let r = engine
            .admit_singleton_respondent(p2, test_manifest("org.test.ping"))
            .await;
        assert!(matches!(r, Err(StewardError::Admission(_))));
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn handle_request_dispatches() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let req = Request {
            request_type: "ping".into(),
            payload: b"hello".to_vec(),
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let resp = engine
            .router()
            .handle_request("test.ping", req)
            .await
            .unwrap();
        assert_eq!(resp.payload, b"hello");
    }

    #[tokio::test]
    async fn handle_request_unknown_shelf_errors() {
        let engine = test_engine();
        let req = Request {
            request_type: "ping".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let r = engine.router().handle_request("missing.shelf", req).await;
        assert!(matches!(r, Err(StewardError::Dispatch(_))));
    }

    #[tokio::test]
    async fn shutdown_unloads_everything() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();
        assert_eq!(engine.len(), 1);
        engine.shutdown().await.unwrap();
        assert_eq!(engine.len(), 0);
    }

    // A respondent that announces subjects during load() for testing
    // the subject-registry wiring end-to-end.
    struct AnnouncingRespondent {
        name: String,
        announcements: Vec<evo_plugin_sdk::contract::SubjectAnnouncement>,
    }

    impl Plugin for AnnouncingRespondent {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["ping".into()],
                        course_correct_verbs: vec![],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.1".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move {
                for announcement in &self.announcements {
                    ctx.subject_announcer
                        .announce(announcement.clone())
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!(
                                "subject announce failed: {e}"
                            ))
                        })?;
                }
                Ok(())
            }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Respondent for AnnouncingRespondent {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move { Ok(Response::for_request(req, req.payload.clone())) }
        }
    }

    #[tokio::test]
    async fn plugin_subject_announcements_reach_the_registry() {
        use evo_plugin_sdk::contract::{
            ExternalAddressing, SubjectAnnouncement,
        };

        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let announcements = vec![
            SubjectAnnouncement::new(
                "track",
                vec![ExternalAddressing::new("mpd-path", "/music/a.flac")],
            ),
            SubjectAnnouncement::new(
                "track",
                vec![ExternalAddressing::new("mpd-path", "/music/b.flac")],
            ),
        ];

        let plugin = AnnouncingRespondent {
            name: "org.test.ping".into(),
            announcements,
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        // Verify the registry saw the announcements.
        let registry = engine.registry();
        assert_eq!(registry.subject_count(), 2);
        assert_eq!(registry.addressing_count(), 2);

        let first = registry
            .resolve(&evo_plugin_sdk::contract::ExternalAddressing::new(
                "mpd-path",
                "/music/a.flac",
            ))
            .unwrap();
        let record = registry.describe(&first).unwrap();
        assert_eq!(record.subject_type, "track");
        assert_eq!(record.addressings.len(), 1);
        assert_eq!(record.addressings[0].claimant, "org.test.ping");
    }

    #[tokio::test]
    async fn shared_registry_in_state_is_visible_to_admission() {
        use evo_plugin_sdk::contract::{
            ExternalAddressing, SubjectAnnouncement,
        };

        let shared = Arc::new(SubjectRegistry::new());
        // Pre-populate the registry outside the engine.
        shared
            .announce(
                &SubjectAnnouncement::new(
                    "track",
                    vec![ExternalAddressing::new("pre", "populated")],
                ),
                "operator",
            )
            .unwrap();

        let catalogue = test_catalogue();
        let state = StewardState::builder()
            .catalogue(catalogue)
            .subjects(Arc::clone(&shared))
            .relations(Arc::new(RelationGraph::new()))
            .custody(Arc::new(CustodyLedger::new()))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(Arc::new(
                crate::persistence::MemoryPersistenceStore::new(),
            ))
            .claimant_issuer(Arc::new(
                crate::claimant::ClaimantTokenIssuer::new("test-instance"),
            ))
            .build()
            .expect("state must build with all handles");
        let mut engine = engine_with_state(state);
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        // Engine sees the externally-populated registry.
        assert_eq!(engine.registry().subject_count(), 1);
        assert_eq!(shared.subject_count(), 1);
    }

    // A respondent that announces two subjects and asserts a relation
    // between them during load, for exercising the full subject +
    // relation wiring end-to-end.
    struct RelatingRespondent {
        name: String,
    }

    impl Plugin for RelatingRespondent {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["ping".into()],
                        course_correct_verbs: vec![],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.1".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            use evo_plugin_sdk::contract::{
                ExternalAddressing, RelationAssertion, SubjectAnnouncement,
            };
            async move {
                ctx.subject_announcer
                    .announce(SubjectAnnouncement::new(
                        "track",
                        vec![ExternalAddressing::new("s", "track-1")],
                    ))
                    .await
                    .map_err(|e| {
                        PluginError::Permanent(format!("announce: {e}"))
                    })?;
                ctx.subject_announcer
                    .announce(SubjectAnnouncement::new(
                        "album",
                        vec![ExternalAddressing::new("s", "album-1")],
                    ))
                    .await
                    .map_err(|e| {
                        PluginError::Permanent(format!("announce: {e}"))
                    })?;
                ctx.relation_announcer
                    .assert(RelationAssertion::new(
                        ExternalAddressing::new("s", "track-1"),
                        "album_of",
                        ExternalAddressing::new("s", "album-1"),
                    ))
                    .await
                    .map_err(|e| {
                        PluginError::Permanent(format!("assert: {e}"))
                    })?;
                Ok(())
            }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Respondent for RelatingRespondent {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move { Ok(Response::for_request(req, req.payload.clone())) }
        }
    }

    #[tokio::test]
    async fn plugin_relation_assertions_reach_the_graph() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let plugin = RelatingRespondent {
            name: "org.test.ping".into(),
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let registry = engine.registry();
        let graph = engine.relation_graph();
        assert_eq!(registry.subject_count(), 2);
        assert_eq!(graph.relation_count(), 1);

        let source_id = registry
            .resolve(&evo_plugin_sdk::contract::ExternalAddressing::new(
                "s", "track-1",
            ))
            .unwrap();
        let target_id = registry
            .resolve(&evo_plugin_sdk::contract::ExternalAddressing::new(
                "s", "album-1",
            ))
            .unwrap();
        assert!(graph.exists(&source_id, "album_of", &target_id));

        let record = graph
            .describe_relation(&source_id, "album_of", &target_id)
            .unwrap();
        assert_eq!(record.claims.len(), 1);
        assert_eq!(record.claims[0].claimant, "org.test.ping");
    }

    // -----------------------------------------------------------------
    // Tests for admit_out_of_process_from_directory error paths.
    //
    // These exercise the cheap failure paths that do not require a
    // real plugin binary. End-to-end happy-path coverage lives in
    // crates/evo-example-echo/tests/out_of_process.rs where a real
    // binary is available via env!(CARGO_BIN_EXE_echo-wire).
    // -----------------------------------------------------------------

    /// Catalogue containing the `example.echo` shelf, matching the
    /// manifests used by the out-of-process error-path tests below.
    fn example_catalogue() -> Arc<Catalogue> {
        Arc::new(
            Catalogue::from_toml(
                r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
charter = "example rack for admission tests"

[[racks.shelves]]
name = "echo"
shape = 1
description = "echo plugin shelf"
"#,
            )
            .unwrap(),
        )
    }

    /// Base manifest string for the example echo plugin. The caller
    /// substitutes the `transport` block to produce variants.
    fn example_manifest_with_transport(transport_block: &str) -> String {
        format!(
            r#"
[plugin]
name = "org.evo.example.echo"
version = "0.1.1"
contract = 1

[target]
shelf = "example.echo"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

{transport_block}

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000
"#
        )
    }

    /// Base manifest string for an example warden plugin. Targets
    /// the `example.echo` shelf for convenience (the shelf is
    /// neutral about kind). The caller substitutes the `transport`
    /// block to produce variants.
    fn example_warden_manifest_with_transport(transport_block: &str) -> String {
        format!(
            r#"
[plugin]
name = "org.evo.example.warden"
version = "0.1.1"
contract = 1

[target]
shelf = "example.echo"
shape = 1

[kind]
instance = "singleton"
interaction = "warden"

{transport_block}

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.warden]
custody_domain = "test"
custody_exclusive = false
course_correction_budget_ms = 1000
custody_failure_mode = "abort"
"#
        )
    }

    /// Write a minimal `privileges.yaml` alongside a manifest so the
    /// admission-time OS-dependency parity gate has a contract to
    /// read. `has_os_dependencies: false` combined with empty
    /// `required_*` vectors passes the SDK validator and the parity
    /// gate. Used by admission tests that drive a bundle past the
    /// early prerequisite checks; tests that expect the parity gate
    /// to refuse write their own privileges.yaml.
    fn write_minimal_privileges_yaml(dir: &std::path::Path, plugin_name: &str) {
        let yaml = format!(
            r#"schema_version: "1.0"
plugin: {plugin_name}
owner: test-fixture
isolation: oop
has_os_dependencies: false
capability_intent: []
required_binaries: []
required_kernel_modules: []
required_system_services: []
verification:
  commands: ["true"]
  expected: ["fixture: never runs"]
host_provisioning: {{}}
"#
        );
        std::fs::write(dir.join("privileges.yaml"), yaml).unwrap();
    }

    #[tokio::test]
    async fn admit_from_directory_rejects_missing_manifest() {
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        let catalogue = example_catalogue();

        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
            )
            .await;

        match r {
            Err(StewardError::Io { context, .. }) => {
                assert!(
                    context.contains("manifest.toml"),
                    "expected context to mention manifest.toml, got {context:?}"
                );
            }
            other => panic!("expected Io error, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_from_directory_rejects_in_process_manifest() {
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        let manifest_text = example_manifest_with_transport(
            "[transport]\ntype = \"in-process\"\nexec = \"<compiled-in>\"",
        );
        std::fs::write(plugin_dir.path().join("manifest.toml"), &manifest_text)
            .unwrap();

        let catalogue = example_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
            )
            .await;

        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("out-of-process"),
                    "expected message to mention out-of-process, got {msg:?}"
                );
            }
            other => panic!("expected Admission error, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_from_directory_reports_spawn_failure() {
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        // Point at a binary that definitely does not exist.
        let manifest_text = example_manifest_with_transport(
            "[transport]\ntype = \"out-of-process\"\nexec = \"nonexistent-plugin-binary-xyz\"",
        );
        std::fs::write(plugin_dir.path().join("manifest.toml"), &manifest_text)
            .unwrap();
        write_minimal_privileges_yaml(
            plugin_dir.path(),
            "org.evo.example.respondent",
        );

        let catalogue = example_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
            )
            .await;

        match r {
            Err(StewardError::Io { context, .. }) => {
                assert!(
                    context.contains("spawning"),
                    "expected context to mention spawning, got {context:?}"
                );
            }
            other => panic!("expected Io error, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_from_directory_warden_manifest_reaches_spawn_step() {
        // admit_out_of_process_from_directory branches on
        // manifest.kind.interaction. This test verifies that warden
        // manifests get past the early validation (manifest parse,
        // transport.kind check) and reach the spawn step. True
        // end-to-end branch coverage (warden manifest routed to
        // admit_out_of_process_warden, describe handshake, load)
        // lives in the example-warden integration tests, where a
        // real warden binary is available.
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        let manifest_text = example_warden_manifest_with_transport(
            "[transport]\ntype = \"out-of-process\"\nexec = \"nonexistent-warden-binary-xyz\"",
        );
        std::fs::write(plugin_dir.path().join("manifest.toml"), &manifest_text)
            .unwrap();
        write_minimal_privileges_yaml(
            plugin_dir.path(),
            "org.evo.example.warden",
        );

        let catalogue = example_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
            )
            .await;

        match r {
            Err(StewardError::Io { context, .. }) => {
                assert!(
                    context.contains("spawning"),
                    "expected context to mention spawning, got {context:?}"
                );
            }
            other => panic!("expected Io error, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    // -----------------------------------------------------------------
    // Warden admission and dispatch tests.
    //
    // Exercise the in-process warden path:
    // - admit_singleton_warden happy path
    // - rejection of cross-kind misuse (respondent manifest passed
    //   to admit_singleton_warden, warden manifest passed to
    //   admit_singleton_respondent)
    // - dispatch routing (take_custody / course_correct /
    //   release_custody go through, handle_request on a warden shelf
    //   is refused, take_custody on a respondent shelf is refused)
    // -----------------------------------------------------------------

    /// A minimal warden for dispatch tests.
    ///
    /// Every verb returns success with predictable outputs. The
    /// returned [`CustodyHandle`] uses the plugin's own name as the
    /// handle id so tests can tell apart multiple admitted wardens
    /// if they ever need to.
    #[derive(Default)]
    struct TestWarden {
        name: String,
    }

    impl Plugin for TestWarden {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec![],
                        course_correct_verbs: vec![],
                        accepts_custody: true,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.1".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Warden for TestWarden {
        fn take_custody<'a>(
            &'a mut self,
            _assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a
        {
            let name = self.name.clone();
            async move { Ok(CustodyHandle::new(name)) }
        }

        fn course_correct<'a>(
            &'a mut self,
            _handle: &'a CustodyHandle,
            _correction: CourseCorrection,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn release_custody<'a>(
            &'a mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }
    }

    /// Warden manifest targeting the `test.custody` shelf from
    /// [`test_catalogue`].
    fn test_warden_manifest(name: &str) -> Manifest {
        let toml = format!(
            r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "test.custody"
shape = 1

[kind]
instance = "singleton"
interaction = "warden"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.warden]
custody_domain = "test"
custody_exclusive = false
course_correction_budget_ms = 1000
custody_failure_mode = "abort"
"#
        );
        Manifest::from_toml(&toml).unwrap()
    }

    #[tokio::test]
    async fn admits_valid_warden() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn admit_singleton_warden_rejects_respondent_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        // The warden here is a Warden-implementing Rust type, but the
        // manifest says interaction = respondent. The kind check must
        // catch this mismatch.
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        let r = engine
            .admit_singleton_warden(
                warden,
                // test_manifest targets test.ping with
                // interaction=respondent.
                test_manifest("org.test.custody"),
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("warden"),
                    "expected message to mention warden, got {msg:?}"
                );
            }
            other => panic!("expected Admission error, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_singleton_respondent_rejects_warden_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        // TestRespondent passed to admit_singleton_respondent with a
        // warden manifest. The kind check must catch this.
        let plugin = TestRespondent {
            name: "org.test.custody".into(),
            ..Default::default()
        };
        let r = engine
            .admit_singleton_respondent(
                plugin,
                test_warden_manifest("org.test.custody"),
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("respondent"),
                    "expected message to mention respondent, got {msg:?}"
                );
            }
            other => panic!("expected Admission error, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    /// Build a respondent manifest with `kind.instance = "factory"`.
    /// Used by the factory-gate tests; structurally identical to
    /// `test_manifest` apart from the instance field.
    fn factory_respondent_manifest(name: &str) -> Manifest {
        let toml = format!(
            r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "test.ping"
shape = 1

[kind]
instance = "factory"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 1000

[capabilities.factory]
max_instances = 4
instance_ttl_seconds = 60
"#
        );
        Manifest::from_toml(&toml).unwrap()
    }

    #[tokio::test]
    async fn admit_singleton_respondent_rejects_factory_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.factory".into(),
            ..Default::default()
        };
        let r = engine
            .admit_singleton_respondent(
                plugin,
                factory_respondent_manifest("org.test.factory"),
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                let lower = msg.to_lowercase();
                assert!(
                    lower.contains("factory") && lower.contains("singleton"),
                    "expected refusal naming factory + singleton, got: {msg}"
                );
            }
            other => {
                panic!("expected Admission(factory ...) error, got {other:?}")
            }
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_singleton_respondent_rejects_theme_manifest() {
        // A theme-kind manifest reaching the functional
        // admission path is operator authoring error: themes
        // load through the artefact admission paths added in
        // the active-selection substrate. The functional
        // entry refuses with a structured diagnostic naming
        // the offending plugin.kind.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.theme".into(),
            ..Default::default()
        };
        let theme_manifest_toml = r#"
[plugin]
name = "org.test.theme"
version = "0.1.0"
contract = 1
kind = "theme"

[target]
shelf = "system.appearance.themes"
shape = 1

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[theme]
display_name = "Test Theme"
"#;
        let manifest = Manifest::from_toml(theme_manifest_toml)
            .expect("theme manifest must parse");
        let r = engine.admit_singleton_respondent(plugin, manifest).await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("admit_singleton_respondent")
                        && msg.to_lowercase().contains("theme"),
                    "expected refusal naming the entry point and the \
                     offending kind, got: {msg}"
                );
            }
            other => {
                panic!("expected Admission(theme refused) error, got {other:?}")
            }
        }
        assert_eq!(engine.len(), 0);
    }

    fn theme_manifest_text(plugin_name: &str) -> String {
        format!(
            r#"
[plugin]
name = "{plugin_name}"
version = "0.1.0"
contract = 1
kind = "theme"

[target]
shelf = "system.appearance.themes"
shape = 1

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[theme]
display_name = "Test Theme"
variants = ["light", "dark"]
"#
        )
    }

    fn ui_shell_manifest_text(plugin_name: &str) -> String {
        format!(
            r#"
[plugin]
name = "{plugin_name}"
version = "0.1.0"
contract = 1
kind = "ui_shell"

[target]
shelf = "system.ui.shell"
shape = 1

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[ui_shell]
shell_type = "web_bundle"
entry_point = "index.html"
required_widget_kinds = ["evo.*"]
supports_themes = true
supports_offline = false
min_evo_version = "0.1.13"
"#
        )
    }

    fn widget_pack_manifest_text(plugin_name: &str) -> String {
        format!(
            r#"
[plugin]
name = "{plugin_name}"
version = "0.1.0"
contract = 1
kind = "widget_kind_pack"

[target]
shelf = "system.ui.widgets"
shape = 1

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[widgets]
provides = ["audio.eq.parametric"]
size_envelopes_path = "size_envelopes.toml"
accessibility_declarations_path = "a11y.toml"
"#
        )
    }

    fn widget_pack_size_envelopes_text() -> &'static str {
        r#"
["audio.eq.parametric"]
id = "audio.eq.parametric"
min_size = "third"
ideal_size = "half"
max_size = "full"
mode = "inline"
schema_version = 1
"#
    }

    fn widget_pack_a11y_text() -> &'static str {
        r#"
["audio.eq.parametric"]
kind_id = "audio.eq.parametric"
aria_role = "slider"
aria_label_source = "label_prop"
contrast = "aaa"
keyboard = { focusable = true, interactions = [] }
screen_reader = { announces = ["audio.eq.band.gain.changed"] }
motion = { animates = false, honours_prefers_reduced_motion = true }
"#
    }

    /// Build an artefact bundle on disk under a tempdir
    /// (manifest + optional widget-pack side files).
    fn build_artefact_bundle(
        manifest_text: &str,
        widget_pack_files: Option<(&str, &str)>,
    ) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("manifest.toml"), manifest_text)
            .expect("write manifest");
        if let Some((envelopes, a11y)) = widget_pack_files {
            std::fs::write(tmp.path().join("size_envelopes.toml"), envelopes)
                .expect("write size envelopes");
            std::fs::write(tmp.path().join("a11y.toml"), a11y)
                .expect("write a11y");
        }
        tmp
    }

    #[tokio::test]
    async fn admit_theme_registers_into_theme_registry() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("theme bundle must admit");

        assert!(themes.contains("com.example.theme").await);
        let admitted = themes.get("com.example.theme").await.unwrap();
        assert_eq!(
            admitted.section.display_name.as_deref(),
            Some("Test Theme")
        );
    }

    #[tokio::test]
    async fn unadmit_theme_emits_unadmitted_happening_and_drops_origin() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("theme bundle must admit");

        // Capture origin recording.
        let origin_before = engine.plugin_origin("com.example.theme").is_some();
        assert!(origin_before, "admit must record plugin_origin");

        let mut subscriber = engine.state().bus.subscribe();
        engine
            .unadmit_theme("com.example.theme")
            .await
            .expect("unadmit must succeed");

        assert!(!themes.contains("com.example.theme").await);
        assert!(
            engine.plugin_origin("com.example.theme").is_none(),
            "unadmit must drop the recorded origin"
        );

        // The unadmit emission lands; consume happenings
        // until we see it (other unrelated emissions may
        // precede it depending on engine state).
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            if std::time::Instant::now() >= deadline {
                panic!("timeout waiting for UiThemeUnadmitted");
            }
            let h = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                subscriber.recv(),
            )
            .await
            .expect("recv must not time out within deadline")
            .expect("recv ok");
            if let crate::happenings::Happening::UiThemeUnadmitted {
                plugin,
                ..
            } = h
            {
                assert_eq!(plugin, "com.example.theme");
                break;
            }
        }
    }

    #[tokio::test]
    async fn unadmit_active_theme_auto_clears_active_selection() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        let active = crate::ui_active::ActiveUiSelection::new()
            .with_themes(Arc::clone(&themes))
            .with_happenings(Arc::clone(&engine.state().bus));
        let active = Arc::new(active);
        engine = engine
            .with_ui_themes(Arc::clone(&themes))
            .with_ui_active(Arc::clone(&active));

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("theme bundle must admit");

        // Activate the theme.
        active
            .set_active_theme(Some("com.example.theme"), "peer:1000")
            .await
            .expect("activate must succeed");
        assert_eq!(
            active.active_theme().await,
            Some("com.example.theme".into())
        );

        engine
            .unadmit_theme("com.example.theme")
            .await
            .expect("unadmit must succeed");

        // Auto-clear: the active slot is now None.
        assert_eq!(
            active.active_theme().await,
            None,
            "unadmitting an active theme must auto-clear the slot"
        );
    }

    #[tokio::test]
    async fn unadmit_inactive_theme_leaves_active_selection_alone() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        let active = crate::ui_active::ActiveUiSelection::new()
            .with_themes(Arc::clone(&themes))
            .with_happenings(Arc::clone(&engine.state().bus));
        let active = Arc::new(active);
        engine = engine
            .with_ui_themes(Arc::clone(&themes))
            .with_ui_active(Arc::clone(&active));

        // Admit two themes, activate one.
        let bundle_a = build_artefact_bundle(
            &theme_manifest_text("com.example.theme.a"),
            None,
        );
        let bundle_b = build_artefact_bundle(
            &theme_manifest_text("com.example.theme.b"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle_a.path())
            .await
            .unwrap();
        engine
            .admit_artefact_from_directory(bundle_b.path())
            .await
            .unwrap();
        active
            .set_active_theme(Some("com.example.theme.a"), "peer:1000")
            .await
            .unwrap();

        // Unadmit the OTHER theme; active slot must NOT be cleared.
        engine.unadmit_theme("com.example.theme.b").await.unwrap();

        assert_eq!(
            active.active_theme().await,
            Some("com.example.theme.a".into()),
            "unadmitting an inactive theme must not touch the active \
             slot"
        );
    }

    #[tokio::test]
    async fn unadmit_widget_kind_pack_rolls_back_kinds() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let widgets = crate::ui_registry::WidgetKindRegistry::shared();
        let packs = crate::ui_registry::WidgetKindPackRegistry::shared();
        engine = engine
            .with_ui_widgets(Arc::clone(&widgets))
            .with_ui_widget_packs(Arc::clone(&packs));

        let bundle = build_artefact_bundle(
            &widget_pack_manifest_text("com.example.widgets"),
            Some((widget_pack_size_envelopes_text(), widget_pack_a11y_text())),
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("widget pack must admit");
        assert!(widgets.contains("audio.eq.parametric").await);

        engine
            .unadmit_widget_kind_pack("com.example.widgets")
            .await
            .expect("unadmit must succeed");

        assert!(
            !widgets.contains("audio.eq.parametric").await,
            "unadmit must roll back the pack's kinds"
        );
        assert!(
            !packs.contains("com.example.widgets").await,
            "unadmit must drop the pack from the pack registry"
        );
    }

    #[tokio::test]
    async fn uninstall_plugin_dispatches_to_unadmit_artefact() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("theme bundle must admit");

        let outcome = engine
            .uninstall_plugin("com.example.theme", None, false)
            .await
            .expect("uninstall must succeed");

        assert_eq!(outcome.plugin, "com.example.theme");
        assert!(outcome.was_currently_admitted);
        assert!(outcome.change_applied);
        assert!(!themes.contains("com.example.theme").await);
        assert!(engine.plugin_origin("com.example.theme").is_none());
    }

    #[tokio::test]
    async fn unadmit_artefact_refuses_unknown_plugin() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        match engine.unadmit_artefact("com.example.ghost").await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("not admitted in any artefact registry"),
                    "expected diagnostic naming the registry sweep, got: {msg}"
                );
            }
            other => panic!("expected Admission(unknown), got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Sub-primitive E: distribution + update integration verification
    // for artefact bundles. Mirror of the functional-plugin trust
    // tests above; verifies the artefact admit paths run signature
    // verification through the same trust gauntlet.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn admit_theme_without_trust_skips_signature_check() {
        // Engine with no plugin_trust: theme admit must not run a
        // signature check; the bundle admits cleanly without a
        // manifest.sig file.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("trust=None must admit cleanly");
        assert!(themes.contains("com.example.theme").await);
    }

    #[tokio::test]
    async fn admit_theme_with_trust_rejects_unsigned_bundle() {
        // unsigned_trust_disallowed = real trust state with
        // allow_unsigned=false. The artefact admit path must refuse
        // a bundle without manifest.sig.
        let catalogue = test_catalogue();
        let mut engine = test_engine_with_trust(
            Arc::clone(&catalogue),
            unsigned_trust_disallowed(),
        );
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        match engine.admit_artefact_from_directory(bundle.path()).await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("signed bundle required"),
                    "expected UnsignedInadmissible wording, got: {msg}"
                );
            }
            other => panic!("expected Admission error, got {other:?}"),
        }
        assert!(!themes.contains("com.example.theme").await);
    }

    #[tokio::test]
    async fn admit_theme_with_trust_accepts_unsigned_when_allowed() {
        // allow_unsigned = true downgrades the manifest's declared
        // trust class to Sandbox. The bundle admits, the registry
        // contains the theme, and the AdmittedTheme records the
        // downgrade.
        let catalogue = test_catalogue();
        let mut engine = test_engine_with_trust(
            Arc::clone(&catalogue),
            unsigned_trust_allowed(),
        );
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("allow_unsigned=true must admit at Sandbox");
        assert!(themes.contains("com.example.theme").await);
    }

    #[tokio::test]
    async fn admit_theme_with_trust_rejects_revoked_digest() {
        // Compute the install digest using the V2 artefact payload
        // (manifest + content-tree digest), put it in a revocations
        // set, and confirm admit refuses.
        let catalogue = test_catalogue();
        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        let id = evo_trust::install_digest_artefact(
            &bundle.path().join("manifest.toml"),
            bundle.path(),
        )
        .expect("digest must compute");

        let revocations_dir = tempfile::tempdir().unwrap();
        let revs_path = revocations_dir.path().join("revocations.toml");
        let body = format!(
            "[[revoke]]\ndigest = \"{}\"\nreason = \"test\"\n",
            evo_trust::format_digest_sha256_hex(&id),
        );
        std::fs::write(&revs_path, body).unwrap();
        let revocations = evo_trust::RevocationSet::load(&revs_path).unwrap();

        let trust = Arc::new(crate::plugin_trust::PluginTrustState {
            keys: Vec::new(),
            revocations,
            options: evo_trust::TrustOptions {
                allow_unsigned: true,
                degrade_trust: true,
            },
        });

        let mut engine = test_engine_with_trust(Arc::clone(&catalogue), trust);
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        match engine.admit_artefact_from_directory(bundle.path()).await {
            Err(StewardError::Admission(msg)) => {
                let lower = msg.to_lowercase();
                assert!(
                    lower.contains("revoked"),
                    "expected revoked diagnostic, got: {msg}"
                );
            }
            other => panic!("expected Admission(revoked), got {other:?}"),
        }
        assert!(!themes.contains("com.example.theme").await);
    }

    #[tokio::test]
    async fn admit_widget_pack_with_trust_rejects_unsigned_bundle() {
        // Widget pack admits run the same trust gauntlet — confirm
        // the artefact-trust plumbing applies uniformly across
        // theme / ui_shell / widget_kind_pack admit paths.
        let catalogue = test_catalogue();
        let mut engine = test_engine_with_trust(
            Arc::clone(&catalogue),
            unsigned_trust_disallowed(),
        );
        let widgets = crate::ui_registry::WidgetKindRegistry::shared();
        let packs = crate::ui_registry::WidgetKindPackRegistry::shared();
        engine = engine
            .with_ui_widgets(Arc::clone(&widgets))
            .with_ui_widget_packs(Arc::clone(&packs));

        let bundle = build_artefact_bundle(
            &widget_pack_manifest_text("com.example.widgets"),
            Some((widget_pack_size_envelopes_text(), widget_pack_a11y_text())),
        );
        match engine.admit_artefact_from_directory(bundle.path()).await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("signed bundle required"),
                    "expected UnsignedInadmissible wording, got: {msg}"
                );
            }
            other => panic!("expected Admission error, got {other:?}"),
        }
        // No partial registration: widget kinds + pack registry both empty.
        assert!(widgets.is_empty().await);
        assert!(packs.is_empty().await);
    }

    #[tokio::test]
    async fn artefact_content_tree_digest_changes_with_asset_content() {
        // Cross-substrate verification: the content-tree digest
        // must change when any non-manifest file under plugin_dir
        // changes. This is the property that makes the V2 signing
        // payload tamper-evident over the bundle's asset surface.
        let bundle = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            bundle.path().join("manifest.toml"),
            theme_manifest_text("com.example.theme"),
        )
        .unwrap();
        std::fs::write(bundle.path().join("logo.png"), b"version-A").unwrap();
        let id_a = evo_trust::install_digest_artefact(
            &bundle.path().join("manifest.toml"),
            bundle.path(),
        )
        .unwrap();

        // Mutate the asset; recompute.
        std::fs::write(bundle.path().join("logo.png"), b"version-B").unwrap();
        let id_b = evo_trust::install_digest_artefact(
            &bundle.path().join("manifest.toml"),
            bundle.path(),
        )
        .unwrap();

        assert_ne!(
            id_a, id_b,
            "content-tree digest must change when asset content changes; \
             tamper-evidence is the signing payload's job"
        );
    }

    /// Minimal `UpdateSource` for integration testing: stages
    /// a theme bundle under the supplied stage directory the
    /// way a real source would, then returns. The stage layout
    /// matches what `admit_artefact_from_directory` expects:
    /// a directory containing `manifest.toml` plus any side
    /// files, ready for discovery dispatch.
    struct TestArtefactSource {
        stage_dir: std::path::PathBuf,
        plugin_name: String,
        manifest_text: String,
    }

    impl evo_plugin_sdk::update::UpdateSource for TestArtefactSource {
        fn source_id(&self) -> evo_plugin_sdk::update::SourceId {
            evo_plugin_sdk::update::SourceId::new("test-artefact")
        }

        fn display_name(&self) -> String {
            "Test artefact source".to_string()
        }

        fn capabilities(&self) -> evo_plugin_sdk::update::SourceCapabilities {
            evo_plugin_sdk::update::SourceCapabilities {
                background_check: false,
                atomic_apply: true,
                requires_restart: evo_plugin_sdk::update::RestartLevel::Plugin,
                rollback_supported: false,
                size_estimate: false,
            }
        }

        fn check_for_updates<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<evo_plugin_sdk::update::UpdateAvailable>,
                            evo_plugin_sdk::update::UpdateError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let plugin_name = self.plugin_name.clone();
            Box::pin(async move {
                Ok(vec![evo_plugin_sdk::update::UpdateAvailable {
                    id: evo_plugin_sdk::update::UpdateId::new(format!(
                        "{plugin_name}@0.1.0"
                    )),
                    component: plugin_name,
                    current_version: "0.0.0".to_string(),
                    available_version: "0.1.0".to_string(),
                    changelog_url: None,
                    severity: evo_plugin_sdk::update::UpdateSeverity::Routine,
                    size_bytes: None,
                    requires_restart:
                        evo_plugin_sdk::update::RestartLevel::Plugin,
                    published_at: std::time::SystemTime::UNIX_EPOCH,
                }])
            })
        }

        fn apply_update<'a>(
            &'a self,
            id: &'a evo_plugin_sdk::update::UpdateId,
            _options: &'a evo_plugin_sdk::update::ApplyOptions,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            evo_plugin_sdk::update::UpdateOutcome,
                            evo_plugin_sdk::update::UpdateError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let stage_dir = self.stage_dir.clone();
            let manifest_text = self.manifest_text.clone();
            let plugin_name = self.plugin_name.clone();
            let id = id.clone();
            Box::pin(async move {
                // Stage the bundle directory under
                // <stage_dir>/<plugin_name>/. Mirrors what a
                // real source would do after fetching +
                // verifying the bundle from upstream.
                let bundle_dir = stage_dir.join(&plugin_name);
                std::fs::create_dir_all(&bundle_dir).map_err(|e| {
                    evo_plugin_sdk::update::UpdateError::ApplyFailed(format!(
                        "create_dir_all {}: {e}",
                        bundle_dir.display()
                    ))
                })?;
                std::fs::write(bundle_dir.join("manifest.toml"), manifest_text)
                    .map_err(|e| {
                        evo_plugin_sdk::update::UpdateError::ApplyFailed(
                            format!("write manifest.toml: {e}"),
                        )
                    })?;
                Ok(evo_plugin_sdk::update::UpdateOutcome {
                    id,
                    component: plugin_name,
                    applied_version: "0.1.0".to_string(),
                    restart_initiated:
                        evo_plugin_sdk::update::RestartLevel::Plugin,
                    dry_run: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn artefact_bundle_through_update_source_admits_end_to_end() {
        // End-to-end integration: a fake UpdateSource stages a
        // theme bundle to its stage directory; admission walks
        // the staged path through admit_artefact_from_directory;
        // the theme lands in the registry. Verifies the
        // substrate composition: artefact bundles ride the
        // same distribution pipeline as functional plugins,
        // kind-agnostic at the source layer and kind-aware at
        // the admission layer.
        let stage_root = tempfile::tempdir().expect("stage tempdir");
        let plugin_name = "com.example.theme";

        let source = TestArtefactSource {
            stage_dir: stage_root.path().to_path_buf(),
            plugin_name: plugin_name.to_string(),
            manifest_text: theme_manifest_text(plugin_name),
        };

        // Stage the bundle through the source's apply_update —
        // exactly the path a real UpdateSource would take.
        let id = evo_plugin_sdk::update::UpdateId::new(format!(
            "{plugin_name}@0.1.0"
        ));
        let options = evo_plugin_sdk::update::ApplyOptions::default();
        let outcome = evo_plugin_sdk::update::UpdateSource::apply_update(
            &source, &id, &options,
        )
        .await
        .expect("source must stage the bundle");
        assert_eq!(outcome.applied_version, "0.1.0");

        // The bundle is staged under stage_root/<plugin_name>/.
        // Discovery dispatches by walking stage directories;
        // we drive admit_artefact_from_directory directly
        // against the staged path to verify the composition.
        let bundle_dir = stage_root.path().join(plugin_name);
        assert!(
            bundle_dir.join("manifest.toml").exists(),
            "source must have staged the manifest"
        );

        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        engine
            .admit_artefact_from_directory(&bundle_dir)
            .await
            .expect("staged bundle must admit");

        assert!(
            themes.contains(plugin_name).await,
            "end-to-end: source-staged theme must reach the registry"
        );
        let admitted = themes.get(plugin_name).await.unwrap();
        assert_eq!(
            admitted.section.display_name.as_deref(),
            Some("Test Theme")
        );
    }

    #[tokio::test]
    async fn artefact_bundle_through_update_source_runs_trust_gauntlet() {
        // Same end-to-end pipeline, but with plugin_trust
        // configured to require signed bundles. The fake
        // source stages a bundle without a manifest.sig, so
        // admission must refuse with the unsigned-
        // inadmissible diagnostic — proving the trust
        // verification ALSO applies to source-staged
        // artefact bundles, identically to functional ones.
        let stage_root = tempfile::tempdir().expect("stage tempdir");
        let plugin_name = "com.example.theme";

        let source = TestArtefactSource {
            stage_dir: stage_root.path().to_path_buf(),
            plugin_name: plugin_name.to_string(),
            manifest_text: theme_manifest_text(plugin_name),
        };

        let id = evo_plugin_sdk::update::UpdateId::new(format!(
            "{plugin_name}@0.1.0"
        ));
        let options = evo_plugin_sdk::update::ApplyOptions::default();
        evo_plugin_sdk::update::UpdateSource::apply_update(
            &source, &id, &options,
        )
        .await
        .expect("source must stage the bundle");
        let bundle_dir = stage_root.path().join(plugin_name);

        let catalogue = test_catalogue();
        let mut engine = test_engine_with_trust(
            Arc::clone(&catalogue),
            unsigned_trust_disallowed(),
        );
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        match engine.admit_artefact_from_directory(&bundle_dir).await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("signed bundle required"),
                    "trust gauntlet must apply to source-staged \
                     artefacts; expected UnsignedInadmissible \
                     wording, got: {msg}"
                );
            }
            other => {
                panic!(
                    "trust gauntlet must refuse unsigned source-staged \
                     bundle, got {other:?}"
                )
            }
        }
        assert!(
            !themes.contains(plugin_name).await,
            "no partial admission on trust refusal"
        );
    }

    #[tokio::test]
    async fn artefact_content_tree_digest_excludes_manifest_sig() {
        // Adding / mutating a top-level manifest.sig file must not
        // change the content-tree digest — the signature is the
        // verifier's input, not part of what's signed.
        let bundle = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            bundle.path().join("manifest.toml"),
            theme_manifest_text("com.example.theme"),
        )
        .unwrap();
        std::fs::write(bundle.path().join("logo.png"), b"asset").unwrap();
        let id_no_sig = evo_trust::install_digest_artefact(
            &bundle.path().join("manifest.toml"),
            bundle.path(),
        )
        .unwrap();

        std::fs::write(
            bundle.path().join("manifest.sig"),
            b"fake-signature-bytes",
        )
        .unwrap();
        let id_with_sig = evo_trust::install_digest_artefact(
            &bundle.path().join("manifest.toml"),
            bundle.path(),
        )
        .unwrap();

        assert_eq!(
            id_no_sig, id_with_sig,
            "manifest.sig must be excluded from the content-tree digest"
        );
    }

    #[tokio::test]
    async fn admit_theme_emits_ui_theme_admitted_happening() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        let mut subscriber = engine.state().bus.subscribe();

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("theme bundle must admit");

        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            subscriber.recv(),
        )
        .await
        {
            Ok(Ok(crate::happenings::Happening::UiThemeAdmitted {
                plugin,
                version,
                ..
            })) => {
                assert_eq!(plugin, "com.example.theme");
                assert_eq!(version, "0.1.0");
            }
            Ok(Ok(other)) => {
                panic!("expected UiThemeAdmitted, got {other:?}")
            }
            Ok(Err(e)) => panic!("subscriber error: {e}"),
            Err(_) => panic!("timeout waiting for UiThemeAdmitted"),
        }
    }

    #[tokio::test]
    async fn admit_theme_duplicate_refuses() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let themes = crate::ui_registry::ThemeRegistry::shared();
        engine = engine.with_ui_themes(Arc::clone(&themes));

        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("first admission must succeed");
        match engine.admit_artefact_from_directory(bundle.path()).await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("theme registry refused")
                        && msg.contains("duplicate"),
                    "expected duplicate-refusal diagnostic, got: {msg}"
                );
            }
            other => {
                panic!("expected Admission(duplicate), got {other:?}")
            }
        }
        assert_eq!(themes.len().await, 1);
    }

    #[tokio::test]
    async fn admit_theme_without_registry_refuses_cleanly() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        // Engine has no ThemeRegistry wired; admission must
        // refuse with a clear diagnostic rather than panic.
        let bundle = build_artefact_bundle(
            &theme_manifest_text("com.example.theme"),
            None,
        );
        match engine.admit_artefact_from_directory(bundle.path()).await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("with_ui_themes"),
                    "expected wiring-hint diagnostic, got: {msg}"
                );
            }
            other => panic!("expected Admission(wiring), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admit_ui_shell_registers_into_shell_registry() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let shells = crate::ui_registry::UiShellRegistry::shared();
        engine = engine.with_ui_shells(Arc::clone(&shells));

        let bundle = build_artefact_bundle(
            &ui_shell_manifest_text("com.example.shell"),
            None,
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("ui_shell bundle must admit");

        let admitted = shells.get("com.example.shell").await.unwrap();
        assert_eq!(admitted.section.shell_type, "web_bundle");
        assert_eq!(admitted.section.entry_point, "index.html");
    }

    #[tokio::test]
    async fn admit_widget_kind_pack_registers_kinds_and_pack() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let widgets = crate::ui_registry::WidgetKindRegistry::shared();
        let packs = crate::ui_registry::WidgetKindPackRegistry::shared();
        engine = engine
            .with_ui_widgets(Arc::clone(&widgets))
            .with_ui_widget_packs(Arc::clone(&packs));

        let bundle = build_artefact_bundle(
            &widget_pack_manifest_text("com.example.widgets"),
            Some((widget_pack_size_envelopes_text(), widget_pack_a11y_text())),
        );
        engine
            .admit_artefact_from_directory(bundle.path())
            .await
            .expect("widget_kind_pack bundle must admit");

        assert!(widgets.contains("audio.eq.parametric").await);
        let admitted = packs.get("com.example.widgets").await.unwrap();
        assert_eq!(admitted.section.provides, vec!["audio.eq.parametric"]);
        assert!(admitted.accessibility.contains_key("audio.eq.parametric"));
    }

    #[tokio::test]
    async fn admit_widget_kind_pack_collision_rolls_back() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let widgets = crate::ui_registry::WidgetKindRegistry::shared();
        let packs = crate::ui_registry::WidgetKindPackRegistry::shared();
        engine = engine
            .with_ui_widgets(Arc::clone(&widgets))
            .with_ui_widget_packs(Arc::clone(&packs));

        // Pre-register the kind to trigger the collision path.
        widgets
            .register(evo_plugin_sdk::ui::WidgetKindEnvelope {
                id: "audio.eq.parametric".into(),
                min_size: evo_plugin_sdk::ui::UiSize::Third,
                ideal_size: evo_plugin_sdk::ui::UiSize::Half,
                max_size: evo_plugin_sdk::ui::UiSize::Full,
                aspect_ratio: evo_plugin_sdk::ui::UiAspect::Any,
                responsive: std::collections::BTreeMap::new(),
                mode: evo_plugin_sdk::ui::UiMode::Inline,
                schema_version: 1,
            })
            .await
            .expect("seed register must succeed");
        let pre_count = widgets.len().await;

        let bundle = build_artefact_bundle(
            &widget_pack_manifest_text("com.example.widgets"),
            Some((widget_pack_size_envelopes_text(), widget_pack_a11y_text())),
        );
        match engine.admit_artefact_from_directory(bundle.path()).await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("already registered"),
                    "expected collision diagnostic, got: {msg}"
                );
            }
            other => panic!("expected Admission(collision), got {other:?}"),
        }
        // Pre-existing kinds untouched; pack not admitted.
        assert_eq!(widgets.len().await, pre_count);
        assert!(packs.is_empty().await);
    }

    #[tokio::test]
    async fn admit_widget_kind_pack_set_mismatch_refuses() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let widgets = crate::ui_registry::WidgetKindRegistry::shared();
        let packs = crate::ui_registry::WidgetKindPackRegistry::shared();
        engine = engine
            .with_ui_widgets(Arc::clone(&widgets))
            .with_ui_widget_packs(Arc::clone(&packs));

        // Manifest declares one kind; envelopes file declares
        // a different one — admission refuses with a clear
        // diagnostic naming the offending side.
        let mismatched_envelopes = r#"
["audio.spectrum.live"]
id = "audio.spectrum.live"
min_size = "third"
ideal_size = "half"
max_size = "full"
mode = "inline"
schema_version = 1
"#;
        let bundle = build_artefact_bundle(
            &widget_pack_manifest_text("com.example.widgets"),
            Some((mismatched_envelopes, widget_pack_a11y_text())),
        );
        match engine.admit_artefact_from_directory(bundle.path()).await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("size envelopes file"),
                    "expected size-envelopes mismatch diagnostic, \
                     got: {msg}"
                );
            }
            other => {
                panic!("expected Admission(set mismatch), got {other:?}")
            }
        }
        assert!(widgets.is_empty().await);
        assert!(packs.is_empty().await);
    }

    #[tokio::test]
    async fn admit_artefact_refuses_functional_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let bundle = tempfile::tempdir().expect("tempdir");
        let functional_manifest = r#"
[plugin]
name = "org.test.ping"
version = "0.1.0"
contract = 1

[target]
shelf = "test.ping"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 1000
"#;
        std::fs::write(
            bundle.path().join("manifest.toml"),
            functional_manifest,
        )
        .expect("write manifest");
        match engine.admit_artefact_from_directory(bundle.path()).await {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains(
                        "admit_artefact_from_directory rejects \
                                  functional plugins"
                    ),
                    "expected functional-rejection diagnostic, got: {msg}"
                );
            }
            other => {
                panic!("expected Admission(functional), got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn take_custody_dispatches_to_warden() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        let handle = engine
            .router()
            .take_custody(
                "test.custody",
                "playback".into(),
                b"payload".to_vec(),
                None,
            )
            .await
            .unwrap();
        // TestWarden uses its own plugin name as the handle id.
        assert_eq!(handle.id, "org.test.custody");
    }

    #[tokio::test]
    async fn course_correct_dispatches_to_warden() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        let handle = engine
            .router()
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();
        engine
            .router()
            .course_correct(
                "test.custody",
                &handle,
                "seek".into(),
                b"pos=42".to_vec(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn release_custody_dispatches_to_warden() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        let handle = engine
            .router()
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();
        engine
            .router()
            .release_custody("test.custody", handle)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn handle_request_on_warden_shelf_errors() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        let req = Request {
            request_type: "ping".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,

            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let r = engine.router().handle_request("test.custody", req).await;
        match r {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("warden"),
                    "expected message to mention warden, got {msg:?}"
                );
            }
            other => panic!("expected Dispatch error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn take_custody_on_respondent_shelf_errors() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(plugin, test_manifest("org.test.ping"))
            .await
            .unwrap();

        let r = engine
            .router()
            .take_custody("test.ping", "playback".into(), vec![], None)
            .await;
        match r {
            Err(StewardError::Dispatch(msg)) => {
                assert!(
                    msg.contains("respondent"),
                    "expected message to mention respondent, got {msg:?}"
                );
            }
            other => panic!("expected Dispatch error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn warden_shutdown_unloads() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();
        assert_eq!(engine.len(), 1);
        engine.shutdown().await.unwrap();
        assert_eq!(engine.len(), 0);
    }

    // -----------------------------------------------------------------
    // Custody ledger integration tests.
    //
    // Verify that take_custody and release_custody on the engine
    // propagate into the engine's CustodyLedger. In-process wardens
    // reach this via AdmissionEngine's own record_custody/
    // release_custody calls; out-of-process wardens additionally
    // reach it via the LedgerCustodyStateReporter installed in the
    // WireWarden's EventSink (tested in wire_client::tests and
    // evo-example-warden integration tests).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn take_custody_records_in_ledger() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();
        assert_eq!(engine.custody_ledger().len(), 0);

        let handle = engine
            .router()
            .take_custody(
                "test.custody",
                "playback".into(),
                b"payload".to_vec(),
                None,
            )
            .await
            .unwrap();

        let ledger = engine.custody_ledger();
        assert_eq!(ledger.len(), 1);
        let rec = ledger
            .describe("org.test.custody", &handle.id)
            .expect("ledger should have recorded the custody");
        assert_eq!(rec.plugin, "org.test.custody");
        assert_eq!(rec.handle_id, handle.id);
        assert_eq!(rec.shelf.as_deref(), Some("test.custody"));
        assert_eq!(rec.custody_type.as_deref(), Some("playback"));
        // TestWarden does not emit state reports during take_custody,
        // so last_state is None until something updates it. A warden
        // that reports would populate this field via the reporter in
        // the Assignment.
        assert!(rec.last_state.is_none());
    }

    #[tokio::test]
    async fn release_custody_removes_from_ledger() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        let handle = engine
            .router()
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();
        assert_eq!(engine.custody_ledger().len(), 1);

        engine
            .router()
            .release_custody("test.custody", handle)
            .await
            .unwrap();
        assert_eq!(engine.custody_ledger().len(), 0);
    }

    #[tokio::test]
    async fn shared_ledger_in_state_is_visible_to_admission() {
        use crate::custody::CustodyLedger;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ledger = Arc::new(CustodyLedger::new());

        let catalogue = test_catalogue();
        let state = StewardState::builder()
            .catalogue(catalogue)
            .subjects(Arc::clone(&registry))
            .relations(Arc::clone(&graph))
            .custody(Arc::clone(&ledger))
            .bus(Arc::new(HappeningBus::new()))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(Arc::new(
                crate::persistence::MemoryPersistenceStore::new(),
            ))
            .claimant_issuer(Arc::new(
                crate::claimant::ClaimantTokenIssuer::new("test-instance"),
            ))
            .build()
            .expect("state must build with all handles");
        let mut engine = engine_with_state(state);

        // Both handles (the externally-held one and the engine's
        // accessor) point at the same underlying ledger.
        assert!(Arc::ptr_eq(&ledger, &engine.custody_ledger()));

        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        let _handle = engine
            .router()
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();

        // Externally-held handle reflects the engine's recording.
        assert_eq!(ledger.len(), 1);
    }

    // -----------------------------------------------------------------
    // Happenings bus integration tests.
    //
    // Verify that take_custody and release_custody emit
    // CustodyTaken / CustodyReleased on the engine's happening bus
    // after the ledger updates. The bus's broadcast semantics are
    // exercised by happenings::tests; these tests assert only on
    // engine-originated emissions.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn take_custody_emits_custody_taken_happening() {
        use crate::happenings::Happening;

        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        // Subscribe BEFORE take_custody so the happening reaches us.
        let mut rx = engine.happening_bus().subscribe();

        let handle = engine
            .router()
            .take_custody(
                "test.custody",
                "playback".into(),
                b"payload".to_vec(),
                None,
            )
            .await
            .unwrap();

        let got = rx.recv().await.expect("recv CustodyTaken");
        match got {
            Happening::CustodyTaken {
                plugin,
                handle_id,
                shelf,
                custody_type,
                ..
            } => {
                assert_eq!(plugin, "org.test.custody");
                assert_eq!(handle_id, handle.id);
                assert_eq!(shelf, "test.custody");
                assert_eq!(custody_type, "playback");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn release_custody_emits_custody_released_happening() {
        use crate::happenings::Happening;

        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        let handle = engine
            .router()
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();

        // Subscribe AFTER the take so only the release reaches us.
        // This also verifies the "late subscriber misses earlier
        // happenings" property at the engine level.
        let mut rx = engine.happening_bus().subscribe();
        let handle_id = handle.id.clone();

        engine
            .router()
            .release_custody("test.custody", handle)
            .await
            .unwrap();

        let got = rx.recv().await.expect("recv CustodyReleased");
        match got {
            Happening::CustodyReleased {
                plugin,
                handle_id: got_id,
                ..
            } => {
                assert_eq!(plugin, "org.test.custody");
                assert_eq!(got_id, handle_id);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn take_then_release_emits_both_in_order() {
        use crate::happenings::Happening;

        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();

        let mut rx = engine.happening_bus().subscribe();

        let handle = engine
            .router()
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();
        engine
            .router()
            .release_custody("test.custody", handle)
            .await
            .unwrap();

        let first = rx.recv().await.expect("recv first");
        assert!(
            matches!(first, Happening::CustodyTaken { .. }),
            "expected CustodyTaken first, got {first:?}"
        );
        let second = rx.recv().await.expect("recv second");
        assert!(
            matches!(second, Happening::CustodyReleased { .. }),
            "expected CustodyReleased second, got {second:?}"
        );
    }

    #[tokio::test]
    async fn engine_built_over_custom_data_root_uses_it_for_load_paths() {
        // The new() constructor takes the per-plugin data root as a
        // dedicated argument. Construct an engine over a tempdir and
        // verify the accessor reflects it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let custom = tmp.path().to_path_buf();

        let state = StewardState::for_tests();
        let engine = AdmissionEngine::new(
            state,
            custom.clone(),
            std::path::PathBuf::new(),
            None,
            PluginsSecurityConfig::default(),
        );
        assert_eq!(engine.plugin_data_root(), custom.as_path());
    }

    #[tokio::test]
    async fn shared_bus_in_state_is_visible_to_admission() {
        use crate::custody::CustodyLedger;
        use crate::happenings::HappeningBus;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());

        let catalogue = test_catalogue();
        let state = StewardState::builder()
            .catalogue(catalogue)
            .subjects(Arc::clone(&registry))
            .relations(Arc::clone(&graph))
            .custody(Arc::clone(&ledger))
            .bus(Arc::clone(&bus))
            .admin(Arc::new(AdminLedger::new()))
            .persistence(Arc::new(
                crate::persistence::MemoryPersistenceStore::new(),
            ))
            .claimant_issuer(Arc::new(
                crate::claimant::ClaimantTokenIssuer::new("test-instance"),
            ))
            .build()
            .expect("state must build with all handles");
        let mut engine = engine_with_state(state);

        // All four shared handles match the engine's accessors.
        assert!(Arc::ptr_eq(&ledger, &engine.custody_ledger()));
        assert!(Arc::ptr_eq(&bus, &engine.happening_bus()));

        // Subscribe through the externally-held bus; the engine's
        // emit should reach us.
        let mut rx = bus.subscribe();

        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
            )
            .await
            .unwrap();
        let _handle = engine
            .router()
            .take_custody("test.custody", "playback".into(), vec![], None)
            .await
            .unwrap();

        let got = rx.recv().await.expect("recv from externally-held bus");
        assert!(matches!(
            got,
            crate::happenings::Happening::CustodyTaken { .. }
        ));
    }

    // -----------------------------------------------------------------
    // Trust-gated admission tests.
    //
    // Verify that the plugin trust state passed to AdmissionEngine::new
    // gates admit_out_of_process_from_directory: without trust state,
    // admission skips signature checks; with trust state, the same
    // bundle is checked against keys and revocations BEFORE spawn.
    // The trust-verification algorithm itself is covered end-to-end
    // in crates/evo-trust/tests/verify.rs; these tests assert only
    // on the steward integration (presence/absence of trust state,
    // error propagation into StewardError).
    // -----------------------------------------------------------------

    fn unsigned_trust_disallowed() -> Arc<crate::plugin_trust::PluginTrustState>
    {
        Arc::new(crate::plugin_trust::PluginTrustState {
            keys: Vec::new(),
            revocations: evo_trust::RevocationSet::default(),
            options: evo_trust::TrustOptions {
                allow_unsigned: false,
                degrade_trust: true,
            },
        })
    }

    fn unsigned_trust_allowed() -> Arc<crate::plugin_trust::PluginTrustState> {
        Arc::new(crate::plugin_trust::PluginTrustState {
            keys: Vec::new(),
            revocations: evo_trust::RevocationSet::default(),
            options: evo_trust::TrustOptions {
                allow_unsigned: true,
                degrade_trust: true,
            },
        })
    }

    // Writes a valid manifest.toml plus a plugin.bin whose content is
    // not a real executable. The trust check reads both files to
    // compute the install digest, then either passes (revocation /
    // unsigned gates) or fails. Downstream spawn will fail on the
    // bogus binary; tests that reach the spawn step assert on that
    // failure as the signal that trust check passed.
    fn write_unsigned_bundle(plugin_dir: &Path) {
        let manifest_text = example_manifest_with_transport(
            "[transport]\ntype = \"out-of-process\"\nexec = \"plugin.bin\"",
        );
        std::fs::write(plugin_dir.join("manifest.toml"), &manifest_text)
            .unwrap();
        std::fs::write(plugin_dir.join("plugin.bin"), b"not-a-binary").unwrap();
    }

    #[tokio::test]
    async fn admit_from_directory_without_trust_skips_signature_check() {
        // Control test: engine constructed with trust=None. The
        // unsigned bundle must reach the spawn step, where it fails
        // on the invalid artefact. Absence of a manifest.sig-related
        // error is the signal that no signature check ran.
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        write_unsigned_bundle(plugin_dir.path());

        let catalogue = example_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        // Engine constructed with trust=None: no signature check runs.

        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
            )
            .await;

        assert!(
            r.is_err(),
            "bogus artefact should fail admission downstream of trust"
        );
        if let Err(StewardError::Admission(msg)) = &r {
            assert!(
                !msg.contains("manifest.sig"),
                "with no trust state, no sig check should run: {msg:?}"
            );
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_from_directory_with_trust_rejects_unsigned_bundle() {
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        write_unsigned_bundle(plugin_dir.path());

        let catalogue = example_catalogue();
        let mut engine = test_engine_with_trust(
            Arc::clone(&catalogue),
            unsigned_trust_disallowed(),
        );

        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
            )
            .await;

        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("signed bundle required"),
                    "expected UnsignedInadmissible wording, got {msg:?}"
                );
            }
            other => {
                panic!("expected Admission error, got {other:?}")
            }
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_from_directory_with_trust_accepts_unsigned_when_allowed() {
        // allow_unsigned = true passes the trust check at Sandbox.
        // Admission then proceeds to spawn, which fails on the
        // invalid binary. The fact that the error is about spawning
        // (not about signatures) is the signal we reached spawn.
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        write_unsigned_bundle(plugin_dir.path());

        let catalogue = example_catalogue();
        let mut engine = test_engine_with_trust(
            Arc::clone(&catalogue),
            unsigned_trust_allowed(),
        );

        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
            )
            .await;

        assert!(r.is_err());
        if let Err(StewardError::Admission(msg)) = &r {
            assert!(
                !msg.contains("signed bundle required"),
                "with allow_unsigned, no sig error should appear: {msg:?}"
            );
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_from_directory_with_trust_rejects_revoked_digest() {
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        write_unsigned_bundle(plugin_dir.path());

        // Compute the install digest and put it in a revocations
        // set. The revocation check fires before the sig-presence
        // check, so allow_unsigned = true must not mask this.
        let id = evo_trust::install_digest(
            &plugin_dir.path().join("manifest.toml"),
            &plugin_dir.path().join("plugin.bin"),
        )
        .unwrap();
        let revocations_dir = tempfile::TempDir::new().unwrap();
        let revs_path = revocations_dir.path().join("revocations.toml");
        let body = format!(
            "[[revoke]]\ndigest = \"{}\"\nreason = \"test\"\n",
            evo_trust::format_digest_sha256_hex(&id),
        );
        std::fs::write(&revs_path, body).unwrap();
        let revocations = evo_trust::RevocationSet::load(&revs_path).unwrap();

        let trust = Arc::new(crate::plugin_trust::PluginTrustState {
            keys: Vec::new(),
            revocations,
            options: evo_trust::TrustOptions {
                allow_unsigned: true,
                degrade_trust: true,
            },
        });

        let catalogue = example_catalogue();
        let mut engine = test_engine_with_trust(Arc::clone(&catalogue), trust);

        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
            )
            .await;

        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    msg.contains("revoked"),
                    "expected revocation error, got {msg:?}"
                );
                assert!(
                    msg.contains("sha256:"),
                    "expected sha256 digest in error, got {msg:?}"
                );
            }
            other => {
                panic!("expected Admission error, got {other:?}")
            }
        }
        assert_eq!(engine.len(), 0);
    }

    // -----------------------------------------------------------------
    // [prerequisites] in-scope enforcement: evo_min_version,
    // os_family.
    //
    // The out-of-scope half (resource caps, outbound_network,
    // filesystem_scopes) is distribution-owned per
    // PLUGIN_PACKAGING.md section 2. Those fields remain
    // parsed-but-advisory in core. Only the two environment-level
    // checks below run at admission.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn admit_refuses_manifest_requiring_future_evo_version() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        // 99.0.0 exceeds any realistic steward version.
        let mut manifest = test_manifest("org.test.ping");
        manifest.prerequisites.evo_min_version = semver::Version::new(99, 0, 0);
        let r = engine.admit_singleton_respondent(plugin, manifest).await;
        match r {
            Err(StewardError::Manifest(
                evo_plugin_sdk::ManifestError::EvoVersionTooLow {
                    required,
                    ..
                },
            )) => {
                assert_eq!(required, semver::Version::new(99, 0, 0));
            }
            other => panic!("expected EvoVersionTooLow, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_accepts_manifest_with_equal_evo_version() {
        // Setting evo_min_version to the steward's own version must
        // admit: the check is strict >, not >=.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let mut manifest = test_manifest("org.test.ping");
        manifest.prerequisites.evo_min_version =
            semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        engine
            .admit_singleton_respondent(plugin, manifest)
            .await
            .expect("equal version must admit");
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn admit_refuses_manifest_with_mismatched_os_family() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        // An OS string that matches no supported host.
        let mut manifest = test_manifest("org.test.ping");
        manifest.prerequisites.os_family = "plan9".to_string();
        let r = engine.admit_singleton_respondent(plugin, manifest).await;
        match r {
            Err(StewardError::Manifest(
                evo_plugin_sdk::ManifestError::OsFamilyMismatch {
                    required,
                    ..
                },
            )) => {
                assert_eq!(required, "plan9");
            }
            other => panic!("expected OsFamilyMismatch, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_accepts_manifest_with_os_family_any() {
        // "any" must pass regardless of host OS. Every fixture
        // above defaults to this; this test pins the behaviour
        // explicitly so a future refactor that narrows "any" is
        // caught.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let mut manifest = test_manifest("org.test.ping");
        manifest.prerequisites.os_family = "any".to_string();
        engine
            .admit_singleton_respondent(plugin, manifest)
            .await
            .expect("os_family = any must always admit");
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn admit_accepts_manifest_with_matching_specific_os_family() {
        // Use std::env::consts::OS as the declared os_family. On
        // Linux this is "linux", on macOS "macos", etc.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let mut manifest = test_manifest("org.test.ping");
        manifest.prerequisites.os_family = std::env::consts::OS.to_string();
        engine
            .admit_singleton_respondent(plugin, manifest)
            .await
            .expect("os_family matching host must admit");
        assert_eq!(engine.len(), 1);
    }

    // -----------------------------------------------------------------
    // Admin-trust gating at admission.
    //
    // check_admin_trust refuses admit if and only if:
    // - manifest.capabilities.admin == true, AND
    // - manifest.trust.class > evo_trust::ADMIN_MINIMUM_TRUST
    //   (strictly less privileged).
    //
    // Recall: lower ordinal = more privileged on TrustClass.
    // Platform (0) < Privileged (1) < Standard (2) < Unprivileged
    // (3) < Sandbox (4). ADMIN_MINIMUM_TRUST is Privileged.
    //
    // Platform and Privileged admin plugins pass the gate;
    // Standard, Unprivileged, and Sandbox admin plugins are
    // refused. Non-admin plugins bypass the gate regardless of
    // class.
    //
    // build_load_context tests verify that admin Arcs are Some for
    // admin plugins and None for non-admin plugins.
    // -----------------------------------------------------------------

    /// Build a test manifest with a given trust class and admin
    /// flag. Used by the admin-trust gating tests below.
    fn test_admin_manifest(
        name: &str,
        trust_class: &str,
        admin: bool,
    ) -> Manifest {
        let admin_line = if admin { "admin = true\n\n" } else { "" };
        let toml = format!(
            r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "test.ping"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "{trust_class}"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities]
{admin_line}[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 1000
"#
        );
        Manifest::from_toml(&toml).unwrap()
    }

    #[tokio::test]
    async fn admission_accepts_admin_plugin_at_platform_class() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let manifest = test_admin_manifest("org.test.ping", "platform", true);
        engine
            .admit_singleton_respondent(plugin, manifest)
            .await
            .expect("platform trust must admit an admin plugin");
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn admission_accepts_admin_plugin_at_privileged_class() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let manifest = test_admin_manifest("org.test.ping", "privileged", true);
        engine
            .admit_singleton_respondent(plugin, manifest)
            .await
            .expect("privileged trust must admit an admin plugin");
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn admission_refuses_admin_plugin_at_standard_class() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let manifest = test_admin_manifest("org.test.ping", "standard", true);
        let r = engine.admit_singleton_respondent(plugin, manifest).await;
        match r {
            Err(StewardError::AdminTrustTooLow {
                plugin_name,
                effective,
                minimum,
            }) => {
                assert_eq!(plugin_name, "org.test.ping");
                assert_eq!(
                    effective,
                    evo_plugin_sdk::manifest::TrustClass::Standard
                );
                assert_eq!(
                    minimum,
                    evo_plugin_sdk::manifest::TrustClass::Privileged
                );
            }
            other => panic!("expected AdminTrustTooLow, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admission_refuses_admin_plugin_at_sandbox_class() {
        // Sandbox is the lowest trust class. Admission must refuse
        // any admin plugin at this class.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let manifest = test_admin_manifest("org.test.ping", "sandbox", true);
        let r = engine.admit_singleton_respondent(plugin, manifest).await;
        assert!(
            matches!(r, Err(StewardError::AdminTrustTooLow { .. })),
            "sandbox admin must be refused, got {r:?}"
        );
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admission_accepts_non_admin_plugin_at_sandbox_class() {
        // Control: a non-admin plugin at sandbox class admits
        // normally. The admin gate bypasses entirely when
        // capabilities.admin = false.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let manifest = test_admin_manifest("org.test.ping", "sandbox", false);
        engine
            .admit_singleton_respondent(plugin, manifest)
            .await
            .expect("non-admin plugin at sandbox must admit");
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn build_load_context_populates_admin_arcs_for_admin_plugin() {
        // Direct unit test on build_load_context with an admin
        // manifest. Both admin Arcs must be Some.
        let catalogue = test_catalogue();
        let manifest = test_admin_manifest("org.test.ping", "platform", true);
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());
        let router = Arc::new(PluginRouter::new(StewardState::for_tests()));
        let data_root = std::path::PathBuf::from("/tmp");
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let conflict_index = Arc::new(SubjectConflictIndex::new());

        let plugins_config_dir = std::path::PathBuf::new();
        let revoked = std::collections::HashSet::new();
        let ctx = build_load_context(
            &data_root,
            &plugins_config_dir,
            &manifest,
            registry,
            graph,
            Arc::clone(&catalogue),
            bus,
            ledger,
            router,
            persistence,
            conflict_index,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::credentials::CredentialChangeBus::new()),
            None,
            Arc::new(crate::online_providers::OnlineProviderConfigBus::new()),
            &revoked,
        )
        .expect("test build_load_context");
        assert!(
            ctx.subject_admin.is_some(),
            "subject_admin must be Some for admin plugin"
        );
        assert!(
            ctx.relation_admin.is_some(),
            "relation_admin must be Some for admin plugin"
        );
    }

    #[tokio::test]
    async fn build_load_context_leaves_admin_arcs_none_for_non_admin_plugin() {
        let catalogue = test_catalogue();
        let manifest = test_manifest("org.test.ping");
        assert!(
            !manifest.capabilities.admin,
            "test_manifest should default admin to false"
        );
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());
        let router = Arc::new(PluginRouter::new(StewardState::for_tests()));
        let data_root = std::path::PathBuf::from("/tmp");
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let conflict_index = Arc::new(SubjectConflictIndex::new());

        let plugins_config_dir = std::path::PathBuf::new();
        let revoked = std::collections::HashSet::new();
        let ctx = build_load_context(
            &data_root,
            &plugins_config_dir,
            &manifest,
            registry,
            graph,
            Arc::clone(&catalogue),
            bus,
            ledger,
            router,
            persistence,
            conflict_index,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::credentials::CredentialChangeBus::new()),
            None,
            Arc::new(crate::online_providers::OnlineProviderConfigBus::new()),
            &revoked,
        )
        .expect("test build_load_context");
        assert!(
            ctx.subject_admin.is_none(),
            "subject_admin must be None for non-admin plugin"
        );
        assert!(
            ctx.relation_admin.is_none(),
            "relation_admin must be None for non-admin plugin"
        );
    }

    #[tokio::test]
    async fn build_load_context_suppresses_admin_arcs_when_admin_revoked() {
        // Operator-revoked capability suppresses the LoadContext
        // handle even though the manifest declares the capability.
        // The effective gate inside `build_load_context` is
        // `manifest.capabilities.X && !revoked.contains("X")`.
        let catalogue = test_catalogue();
        let manifest = test_admin_manifest("org.test.ping", "platform", true);
        assert!(
            manifest.capabilities.admin,
            "test_admin_manifest with admin=true should declare admin"
        );
        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let bus = Arc::new(HappeningBus::new());
        let ledger = Arc::new(AdminLedger::new());
        let router = Arc::new(PluginRouter::new(StewardState::for_tests()));
        let data_root = std::path::PathBuf::from("/tmp");
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(crate::persistence::MemoryPersistenceStore::new());
        let conflict_index = Arc::new(SubjectConflictIndex::new());

        let plugins_config_dir = std::path::PathBuf::new();
        let mut revoked = std::collections::HashSet::new();
        revoked.insert("admin".to_string());

        let ctx = build_load_context(
            &data_root,
            &plugins_config_dir,
            &manifest,
            registry,
            graph,
            Arc::clone(&catalogue),
            bus,
            ledger,
            router,
            persistence,
            conflict_index,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Arc::new(crate::credentials::CredentialChangeBus::new()),
            None,
            Arc::new(crate::online_providers::OnlineProviderConfigBus::new()),
            &revoked,
        )
        .expect("test build_load_context");
        assert!(
            ctx.subject_admin.is_none(),
            "subject_admin must be None when admin capability is revoked, \
             regardless of manifest declaration"
        );
        assert!(
            ctx.relation_admin.is_none(),
            "relation_admin must be None when admin capability is revoked, \
             regardless of manifest declaration"
        );
    }

    #[tokio::test]
    async fn revoked_capabilities_for_returns_empty_set_without_store() {
        // Engines built without a CapabilityGrantStore (in-process
        // test harnesses) treat every plugin as having an empty
        // revoked set, so admission paths that consult the helper
        // see fail-open semantics rather than a panic.
        let catalogue = test_catalogue();
        let engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let revoked = engine
            .revoked_capabilities_for("org.test.never-revoked")
            .await
            .expect("revoked_capabilities_for must not error without store");
        assert!(
            revoked.is_empty(),
            "without a store, every plugin's revoked set is empty"
        );
    }

    #[tokio::test]
    async fn revoked_capabilities_for_returns_recorded_set_with_store() {
        // Engines wired with a CapabilityGrantStore return the
        // recorded set per plugin so the LoadContext builder can
        // suppress matching handles. Assert the round-trip from
        // the store through the helper.
        use crate::capability_grant::CapabilityGrantStore;
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let store = Arc::new(CapabilityGrantStore::new(Arc::clone(
            &engine.state.persistence,
        )));
        store
            .revoke(
                "org.test.with-revocation",
                "outbound_network",
                "test:1000",
                Some("unit test"),
            )
            .await
            .expect("revoke succeeds in-memory");
        engine = engine.with_capability_grant_store(Arc::clone(&store));

        let revoked = engine
            .revoked_capabilities_for("org.test.with-revocation")
            .await
            .expect("revoked_capabilities_for must succeed");
        assert!(
            revoked.contains("outbound_network"),
            "revoked set must include the recorded capability"
        );
        assert_eq!(revoked.len(), 1);

        let other = engine
            .revoked_capabilities_for("org.test.unrelated")
            .await
            .expect("revoked_capabilities_for must succeed");
        assert!(
            other.is_empty(),
            "revocation is per-plugin; unrelated plugin sees empty set"
        );
    }

    // ---------------------------------------------------------------
    // Staged-shutdown tests.
    //
    // These exercise `shutdown_with_config` end-to-end. The catalogue
    // and manifests below add several pingable shelves so multiple
    // respondents can be admitted in parallel without colliding.
    // ---------------------------------------------------------------

    /// Custom catalogue with eight respondent shelves plus the
    /// existing custody shelf; used by the shutdown-stage tests so
    /// multiple plugins can be admitted at once.
    fn shutdown_test_catalogue() -> Arc<Catalogue> {
        Arc::new(
            Catalogue::from_toml(
                r#"
schema_version = 1

[[racks]]
name = "shut"
family = "domain"
charter = "shutdown rack"

[[racks.shelves]]
name = "a"
shape = 1
description = "shelf a"

[[racks.shelves]]
name = "b"
shape = 1
description = "shelf b"

[[racks.shelves]]
name = "c"
shape = 1
description = "shelf c"

[[racks.shelves]]
name = "d"
shape = 1
description = "shelf d"

[[racks.shelves]]
name = "e"
shape = 1
description = "shelf e"

[[racks.shelves]]
name = "f"
shape = 1
description = "shelf f"

[[racks.shelves]]
name = "warden"
shape = 1
description = "shutdown warden"

[[subjects]]
name = "track"
"#,
            )
            .unwrap(),
        )
    }

    /// Build a respondent manifest targeting `shut.<shelf>`.
    fn shutdown_manifest_for_shelf(name: &str, shelf_leaf: &str) -> Manifest {
        let toml = format!(
            r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "shut.{shelf_leaf}"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["ping"]
response_budget_ms = 1000
"#
        );
        Manifest::from_toml(&toml).unwrap()
    }

    /// Build a warden manifest targeting `shut.warden`.
    fn shutdown_warden_manifest(name: &str) -> Manifest {
        let toml = format!(
            r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "shut.warden"
shape = 1

[kind]
instance = "singleton"
interaction = "warden"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.warden]
custody_domain = "test"
custody_exclusive = false
course_correction_budget_ms = 1000
custody_failure_mode = "abort"
"#
        );
        Manifest::from_toml(&toml).unwrap()
    }

    /// Respondent whose `unload()` sleeps for `unload_delay` before
    /// returning. Used to time the parallel-vs-serial unload race.
    struct DelayedUnloadRespondent {
        name: String,
        unload_delay: Duration,
    }

    impl Plugin for DelayedUnloadRespondent {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["ping".into()],
                        course_correct_verbs: vec![],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
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
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            let delay = self.unload_delay;
            async move {
                tokio::time::sleep(delay).await;
                Ok(())
            }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Respondent for DelayedUnloadRespondent {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move { Ok(Response::for_request(req, req.payload.clone())) }
        }
    }

    #[tokio::test]
    async fn shutdown_unloads_all_plugins_in_parallel() {
        // Six plugins each sleep ~50ms in unload(). With serial
        // shutdown that totals ~300ms; in parallel it should finish
        // close to the single-plugin delay. The threshold is
        // generous to absorb scheduling noise on shared CI.
        let catalogue = shutdown_test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let leaves = ["a", "b", "c", "d", "e", "f"];
        for leaf in leaves {
            let plugin = DelayedUnloadRespondent {
                name: format!("org.test.shut.{leaf}"),
                unload_delay: Duration::from_millis(50),
            };
            engine
                .admit_singleton_respondent(
                    plugin,
                    shutdown_manifest_for_shelf(
                        &format!("org.test.shut.{leaf}"),
                        leaf,
                    ),
                )
                .await
                .unwrap();
        }
        assert_eq!(engine.len(), leaves.len());

        let started = Instant::now();
        let report =
            engine.shutdown_with_config(ShutdownConfig::default()).await;
        let elapsed = started.elapsed();

        assert_eq!(engine.len(), 0);
        assert_eq!(report.plugins_total, leaves.len());
        assert_eq!(report.plugins_unloaded_cleanly.len(), leaves.len());
        assert!(report.plugins_killed_after_deadline.is_empty());

        // Serial would be ~300ms; parallel should be well under
        // 200ms even on a slow runner. Use the same generous-bound
        // pattern as the concurrency proof test.
        assert!(
            elapsed < Duration::from_millis(200),
            "parallel shutdown took {elapsed:?}, expected < 200ms"
        );
    }

    /// Respondent whose `unload()` blocks indefinitely (until the
    /// supervising task aborts it). Used to drive the deadline path
    /// in `shutdown_respects_global_deadline_with_kill`.
    struct HangingUnloadRespondent {
        name: String,
    }

    impl Plugin for HangingUnloadRespondent {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["ping".into()],
                        course_correct_verbs: vec![],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
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
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move {
                // Block effectively forever; the orchestrator must
                // abort us via the global deadline.
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Respondent for HangingUnloadRespondent {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move { Ok(Response::for_request(req, req.payload.clone())) }
        }
    }

    #[tokio::test]
    async fn shutdown_respects_global_deadline_with_kill() {
        // One plugin's unload sleeps for 30s; the global deadline is
        // 200ms. The supervisor must abort the task and report the
        // plugin under plugins_killed_after_deadline.
        //
        // The plugin is in-process (no child to SIGKILL); the
        // killed-after-deadline path still names it because its task
        // did not complete in time. This pins the timeout-and-name
        // behaviour without needing an out-of-process spawn.
        let catalogue = shutdown_test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let plugin = HangingUnloadRespondent {
            name: "org.test.shut.hang".into(),
        };
        engine
            .admit_singleton_respondent(
                plugin,
                shutdown_manifest_for_shelf("org.test.shut.hang", "a"),
            )
            .await
            .unwrap();

        let report = engine
            .shutdown_with_config(ShutdownConfig {
                global_deadline: Duration::from_millis(200),
            })
            .await;

        assert_eq!(report.plugins_total, 1);
        assert!(
            report.plugins_unloaded_cleanly.is_empty(),
            "no plugin should have completed unload: {:?}",
            report.plugins_unloaded_cleanly
        );
        assert_eq!(report.plugins_killed_after_deadline.len(), 1);
        assert_eq!(
            report.plugins_killed_after_deadline[0],
            "org.test.shut.hang"
        );
        // The deadline was 200ms; allow a generous upper bound so a
        // slow runner does not flake.
        assert!(
            report.elapsed < Duration::from_secs(5),
            "shutdown elapsed {:?} should be near the 200ms deadline",
            report.elapsed
        );
    }

    #[tokio::test]
    async fn shutdown_drains_custody_within_window() {
        // Admit a warden, take custody, then shutdown. The custody
        // should appear in custody_drained because the warden's
        // release_custody returns Ok within the drain window.
        let catalogue = shutdown_test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let warden = TestWarden {
            name: "org.test.shut.warden".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                shutdown_warden_manifest("org.test.shut.warden"),
            )
            .await
            .unwrap();

        let _h = engine
            .router()
            .take_custody("shut.warden", "playback".into(), vec![], None)
            .await
            .expect("take_custody");
        assert_eq!(engine.custody_ledger().len(), 1);

        let report =
            engine.shutdown_with_config(ShutdownConfig::default()).await;

        assert!(
            report.custody_abandoned.is_empty(),
            "expected no abandoned custodies: {:?}",
            report.custody_abandoned
        );
        assert_eq!(report.custody_drained.len(), 1);
        assert_eq!(report.custody_drained[0].plugin, "org.test.shut.warden");
        // After successful release the ledger must be empty.
        assert_eq!(engine.custody_ledger().len(), 0);
    }

    /// Warden whose `release_custody` always returns
    /// `PluginError::Permanent` — mirrors the live failure mode where
    /// a plugin's `self.custodies` map is empty (e.g. after a steward
    /// restart that rehydrated a ledger row the fresh plugin instance
    /// never owned) and the drain calls release against an unknown
    /// handle.
    struct RejectingReleaseWarden {
        name: String,
    }

    impl Plugin for RejectingReleaseWarden {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec![],
                        course_correct_verbs: vec![],
                        accepts_custody: true,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.1".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Warden for RejectingReleaseWarden {
        fn take_custody<'a>(
            &'a mut self,
            _assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a
        {
            let name = self.name.clone();
            async move { Ok(CustodyHandle::new(name)) }
        }

        fn course_correct<'a>(
            &'a mut self,
            _handle: &'a CustodyHandle,
            _correction: CourseCorrection,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn release_custody<'a>(
            &'a mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move {
                Err(PluginError::Permanent("unknown custody handle".into()))
            }
        }
    }

    #[tokio::test]
    async fn shutdown_clears_ledger_when_release_rejects_unknown_handle() {
        // Reproduces the rig failure mode: a custody row exists in
        // the ledger but the warden's `release_custody` rejects the
        // handle. The shutdown drain must:
        //   1. report the custody under `custody_abandoned` (audit
        //      trail),
        //   2. clear the ledger row so that the persisted custody
        //      record does NOT survive into the next boot's
        //      rehydration — otherwise every subsequent shutdown
        //      produces the same warn line forever.
        //
        // Before this fix the ledger row stayed; with the fix the
        // ledger is empty post-shutdown.
        let catalogue = shutdown_test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let warden = RejectingReleaseWarden {
            name: "org.test.shut.reject".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                shutdown_warden_manifest("org.test.shut.reject"),
            )
            .await
            .unwrap();

        let _h = engine
            .router()
            .take_custody("shut.warden", "playback".into(), vec![], None)
            .await
            .expect("take_custody");
        assert_eq!(engine.custody_ledger().len(), 1);

        let report =
            engine.shutdown_with_config(ShutdownConfig::default()).await;

        assert_eq!(
            report.custody_abandoned.len(),
            1,
            "release rejection must surface as an abandoned entry: {:?}",
            report.custody_abandoned
        );
        assert!(
            report.custody_drained.is_empty(),
            "no custody should be reported as cleanly drained: {:?}",
            report.custody_drained
        );
        assert_eq!(
            engine.custody_ledger().len(),
            0,
            "ledger must be cleared even when warden rejected release \
             — otherwise the row rehydrates next boot and the warn \
             repeats forever"
        );
    }

    #[tokio::test]
    async fn shutdown_returns_report_with_counts_matching_admit() {
        // Admit four respondents; shutdown; the report's
        // plugins_total must equal the admit count, and the sum of
        // unloaded_cleanly + killed_after_deadline must equal total.
        let catalogue = shutdown_test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let leaves = ["a", "b", "c", "d"];
        for leaf in leaves {
            let plugin = DelayedUnloadRespondent {
                name: format!("org.test.shut.{leaf}"),
                unload_delay: Duration::from_millis(5),
            };
            engine
                .admit_singleton_respondent(
                    plugin,
                    shutdown_manifest_for_shelf(
                        &format!("org.test.shut.{leaf}"),
                        leaf,
                    ),
                )
                .await
                .unwrap();
        }
        assert_eq!(engine.len(), leaves.len());

        let report =
            engine.shutdown_with_config(ShutdownConfig::default()).await;

        assert_eq!(report.plugins_total, leaves.len());
        assert_eq!(
            report.plugins_unloaded_cleanly.len()
                + report.plugins_killed_after_deadline.len(),
            report.plugins_total
        );
    }

    // -----------------------------------------------------------------
    // In-process factory admission tests.
    //
    // Exercise admit_factory_respondent and admit_factory_warden:
    // - happy path admits the plugin and routes through the
    //   RegistryInstanceAnnouncer wired into LoadContext
    // - the singleton admit methods refuse factory manifests
    // - the factory admit methods refuse non-factory manifests
    // - the factory admit methods refuse cross-kind manifests
    //   (warden manifest passed to admit_factory_respondent etc.)
    //
    // The announcer's announce/retract semantics (subject mint, policy
    // enforcement, happening emission) are covered in the factory
    // module's own tests; admission tests here only verify the wiring
    // path and the kind-gate enforcement.
    // -----------------------------------------------------------------

    use evo_plugin_sdk::contract::factory::{Factory, RetractionPolicy};

    /// A minimal factory respondent for the admission tests. Stores
    /// the announcer Arc captured during `load` so tests can drive
    /// announcements through the steward-side InstanceAnnouncer
    /// wiring.
    #[derive(Default)]
    struct TestFactoryRespondent {
        name: String,
        policy_choice: Option<RetractionPolicy>,
    }

    impl Plugin for TestFactoryRespondent {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["ping".into()],
                        course_correct_verbs: vec![],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
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
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Respondent for TestFactoryRespondent {
        fn handle_request<'a>(
            &'a self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move { Ok(Response::for_request(req, req.payload.clone())) }
        }
    }

    impl Factory for TestFactoryRespondent {
        fn retraction_policy(&self) -> RetractionPolicy {
            self.policy_choice.unwrap_or(RetractionPolicy::Dynamic)
        }
    }

    /// A minimal factory warden for admission tests. Same shape as
    /// `TestWarden` plus a `Factory` impl with a configurable policy.
    #[derive(Default)]
    struct TestFactoryWarden {
        name: String,
        policy_choice: Option<RetractionPolicy>,
    }

    impl Plugin for TestFactoryWarden {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_ {
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name: self.name.clone(),
                        version: semver::Version::new(0, 1, 0),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec![],
                        course_correct_verbs: vec![],
                        accepts_custody: true,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: "0.1.1".into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            _ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_ {
            async move { HealthReport::healthy() }
        }
    }

    impl Warden for TestFactoryWarden {
        fn take_custody<'a>(
            &'a mut self,
            _assignment: Assignment,
        ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a
        {
            let name = self.name.clone();
            async move { Ok(CustodyHandle::new(name)) }
        }

        fn course_correct<'a>(
            &'a mut self,
            _handle: &'a CustodyHandle,
            _correction: CourseCorrection,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }

        fn release_custody<'a>(
            &'a mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
            async move { Ok(()) }
        }
    }

    impl Factory for TestFactoryWarden {
        fn retraction_policy(&self) -> RetractionPolicy {
            self.policy_choice.unwrap_or(RetractionPolicy::Dynamic)
        }
    }

    fn factory_warden_manifest(name: &str) -> Manifest {
        let toml = format!(
            r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "test.custody"
shape = 1

[kind]
instance = "factory"
interaction = "warden"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.warden]
custody_domain = "playback"
custody_failure_mode = "abort"
custody_exclusive = false
course_correction_budget_ms = 1000

[capabilities.factory]
max_instances = 4
instance_ttl_seconds = 60
"#
        );
        Manifest::from_toml(&toml).unwrap()
    }

    #[tokio::test]
    async fn admit_factory_respondent_admits_factory_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryRespondent {
            name: "org.test.factory.resp".into(),
            policy_choice: Some(RetractionPolicy::Dynamic),
        };
        engine
            .admit_factory_respondent(
                plugin,
                factory_respondent_manifest("org.test.factory.resp"),
            )
            .await
            .expect("factory respondent admits");
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn admit_factory_warden_admits_factory_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryWarden {
            name: "org.test.factory.warden".into(),
            policy_choice: Some(RetractionPolicy::Dynamic),
        };
        engine
            .admit_factory_warden(
                plugin,
                factory_warden_manifest("org.test.factory.warden"),
            )
            .await
            .expect("factory warden admits");
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn admit_factory_respondent_refuses_singleton_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryRespondent {
            name: "org.test.singleton".into(),
            policy_choice: None,
        };
        // test_manifest declares kind.instance = "singleton".
        let r = engine
            .admit_factory_respondent(
                plugin,
                test_manifest("org.test.singleton"),
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    {
                        let lower = msg.to_lowercase();
                        lower.contains("factory") && lower.contains("singleton")
                    },
                    "expected refusal naming factory + Singleton, got: {msg}"
                );
            }
            other => {
                panic!("expected Admission(factory ...) error, got {other:?}")
            }
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_factory_warden_refuses_singleton_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryWarden {
            name: "org.test.singleton.warden".into(),
            policy_choice: None,
        };
        let r = engine
            .admit_factory_warden(
                plugin,
                test_warden_manifest("org.test.singleton.warden"),
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    {
                        let lower = msg.to_lowercase();
                        lower.contains("factory") && lower.contains("singleton")
                    },
                    "expected refusal naming factory + Singleton, got: {msg}"
                );
            }
            other => {
                panic!("expected Admission(factory ...) error, got {other:?}")
            }
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_factory_respondent_refuses_warden_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryRespondent {
            name: "org.test.cross".into(),
            policy_choice: None,
        };
        // factory_warden_manifest declares interaction = "warden".
        let r = engine
            .admit_factory_respondent(
                plugin,
                factory_warden_manifest("org.test.cross"),
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    {
                        let lower = msg.to_lowercase();
                        lower.contains("respondent") && lower.contains("warden")
                    },
                    "expected refusal naming respondent + Warden, got: {msg}"
                );
            }
            other => panic!(
                "expected Admission(interaction mismatch) error, got {other:?}"
            ),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_factory_warden_refuses_respondent_manifest() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryWarden {
            name: "org.test.cross.warden".into(),
            policy_choice: None,
        };
        let r = engine
            .admit_factory_warden(
                plugin,
                factory_respondent_manifest("org.test.cross.warden"),
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    {
                        let lower = msg.to_lowercase();
                        lower.contains("warden") && lower.contains("respondent")
                    },
                    "expected refusal naming warden + Respondent, got: {msg}"
                );
            }
            other => panic!(
                "expected Admission(interaction mismatch) error, got {other:?}"
            ),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn admit_singleton_respondent_still_refuses_factory_manifest() {
        // Sanity check: removing reject_factory_for_v0 from the
        // singleton path replaced it with an inline kind check; the
        // refusal must still happen.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryRespondent {
            name: "org.test.singleton.refuses.factory".into(),
            policy_choice: None,
        };
        let r = engine
            .admit_singleton_respondent(
                plugin,
                factory_respondent_manifest(
                    "org.test.singleton.refuses.factory",
                ),
            )
            .await;
        match r {
            Err(StewardError::Admission(msg)) => {
                assert!(
                    {
                        let lower = msg.to_lowercase();
                        lower.contains("singleton") && lower.contains("factory")
                    },
                    "expected singleton path refuses factory: {msg}"
                );
            }
            other => panic!("expected Admission(factory) error, got {other:?}"),
        }
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn shutdown_drains_factory_instances_before_router_drain() {
        use crate::happenings::Happening;
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        // Subscribe to the bus before admission so we observe every
        // FactoryInstance happening the drain emits.
        let mut subscriber = engine.state().bus.subscribe();

        let plugin = TestFactoryRespondent {
            name: "org.test.factory.drain".into(),
            policy_choice: Some(RetractionPolicy::Dynamic),
        };
        engine
            .admit_factory_respondent(
                plugin,
                factory_respondent_manifest("org.test.factory.drain"),
            )
            .await
            .expect("factory admits");

        // Reach in through the per-plugin map to drive an announce
        // (the test plugin doesn't auto-announce on load so we
        // manually announce a couple of instances to exercise the
        // drain path).
        let announcer = engine
            .factory_announcers
            .lock()
            .unwrap()
            .get("org.test.factory.drain")
            .cloned()
            .expect("announcer registered");

        announcer
            .announce(
                evo_plugin_sdk::contract::factory::InstanceAnnouncement::new(
                    "x",
                    vec![],
                ),
            )
            .await
            .expect("announce x");
        announcer
            .announce(
                evo_plugin_sdk::contract::factory::InstanceAnnouncement::new(
                    "y",
                    vec![],
                ),
            )
            .await
            .expect("announce y");
        assert_eq!(announcer.instance_count(), 2);

        // Drain a couple of announce happenings off the bus so the
        // assertions below see only retract events.
        for _ in 0..2 {
            let _ = subscriber.recv().await;
        }

        // Shut down the engine. Stage 1 of shutdown_with_config drains
        // every registered factory's instances before plugin unload.
        let report =
            engine.shutdown_with_config(ShutdownConfig::default()).await;

        // The plugin itself unloaded cleanly.
        assert_eq!(report.plugins_total, 1);
        assert_eq!(report.plugins_unloaded_cleanly.len(), 1);

        // Both instances retracted, in announce-order's reverse.
        let mut retracted_ids = Vec::new();
        for _ in 0..2 {
            match subscriber.recv().await.expect("retract happening") {
                Happening::FactoryInstanceRetracted { instance_id, .. } => {
                    retracted_ids.push(instance_id)
                }
                other => {
                    panic!("expected FactoryInstanceRetracted, got {other:?}")
                }
            }
        }
        // Reverse alphabetical = LIFO of (x, y) by sort: y first, then x.
        assert_eq!(retracted_ids, vec!["y".to_string(), "x".to_string()]);

        // The announcer's internal map is empty after drain.
        assert_eq!(announcer.instance_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_drain_is_idempotent_for_factories_with_no_instances() {
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryRespondent {
            name: "org.test.factory.empty".into(),
            policy_choice: Some(RetractionPolicy::Dynamic),
        };
        engine
            .admit_factory_respondent(
                plugin,
                factory_respondent_manifest("org.test.factory.empty"),
            )
            .await
            .unwrap();

        // No announce calls. Drain should be a no-op (no retract
        // happenings emitted) and the plugin still unloads cleanly.
        let report =
            engine.shutdown_with_config(ShutdownConfig::default()).await;
        assert_eq!(report.plugins_unloaded_cleanly.len(), 1);
    }

    #[tokio::test]
    async fn shutdown_drains_shutdown_only_factory_instances() {
        use crate::happenings::Happening;
        // ShutdownOnly factories: the plugin can't retract during
        // its lifetime, but the steward's drain path bypasses that
        // gate so instances are still cleanly retracted on unload.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let mut subscriber = engine.state().bus.subscribe();

        let plugin = TestFactoryRespondent {
            name: "org.test.factory.shutdown-only".into(),
            policy_choice: Some(RetractionPolicy::ShutdownOnly),
        };
        engine
            .admit_factory_respondent(
                plugin,
                factory_respondent_manifest("org.test.factory.shutdown-only"),
            )
            .await
            .unwrap();

        let announcer = engine
            .factory_announcers
            .lock()
            .unwrap()
            .get("org.test.factory.shutdown-only")
            .cloned()
            .unwrap();

        announcer
            .announce(
                evo_plugin_sdk::contract::factory::InstanceAnnouncement::new(
                    "live",
                    vec![],
                ),
            )
            .await
            .unwrap();

        // Plugin's own retract during lifetime is refused under
        // ShutdownOnly — sanity check.
        let err = announcer
            .retract(evo_plugin_sdk::contract::factory::InstanceId::from(
                "live",
            ))
            .await
            .expect_err("ShutdownOnly refuses retract during lifetime");
        assert!(matches!(
            err,
            evo_plugin_sdk::contract::ReportError::Invalid(_)
        ));
        assert_eq!(announcer.instance_count(), 1);

        // Drain announce happening so we observe only the retract.
        let _ = subscriber.recv().await;

        // Steward drain DOES retract under ShutdownOnly — that's the
        // whole point of the bypass.
        let _report =
            engine.shutdown_with_config(ShutdownConfig::default()).await;
        match subscriber.recv().await.unwrap() {
            Happening::FactoryInstanceRetracted { instance_id, .. } => {
                assert_eq!(instance_id, "live");
            }
            other => panic!("expected FactoryInstanceRetracted, got {other:?}"),
        }
        assert_eq!(announcer.instance_count(), 0);
    }

    #[tokio::test]
    async fn scrub_factory_orphans_forgets_subjects_with_no_live_announcer() {
        // Hand-construct a factory subject in the registry without
        // registering its announcer in the engine's
        // factory_announcers map. The scrub should treat it as an
        // orphan and forget it; the registry no longer has the
        // subject afterwards.
        use crate::factory::FACTORY_INSTANCE_SCHEME;
        use evo_plugin_sdk::contract::ExternalAddressing;
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let catalogue = test_catalogue();
        let engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        // Mint a factory-shaped subject directly into the registry,
        // bypassing the announcer (simulating a persisted subject from
        // a previous boot whose owning plugin did not re-admit).
        let plugin_name = "org.test.factory.previous_boot";
        let addressing = ExternalAddressing::new(
            FACTORY_INSTANCE_SCHEME,
            format!("{plugin_name}/orphan-1"),
        );
        let announcement = SubjectAnnouncement {
            subject_type: "test.ping".into(),
            addressings: vec![addressing.clone()],
            claims: Vec::new(),
            state: serde_json::Value::Null,
            announced_at: std::time::SystemTime::now(),
        };
        engine
            .state()
            .subjects
            .announce(&announcement, plugin_name)
            .expect("hand-mint orphan subject");

        // Sanity: the subject is in the registry pre-scrub.
        let pre = engine.state().subjects.snapshot_subjects();
        assert_eq!(pre.len(), 1, "exactly one subject pre-scrub");

        // Scrub: no announcers registered, so the orphan is forgotten.
        let report = engine.scrub_factory_orphans().await;
        assert_eq!(report.forgotten, 1);
        assert_eq!(report.errored, 0);

        // Subject is gone from the registry.
        let post = engine.state().subjects.snapshot_subjects();
        assert!(post.is_empty(), "orphan removed; got {post:?}");
    }

    #[tokio::test]
    async fn scrub_factory_orphans_preserves_live_factory_subjects() {
        // Admit a factory plugin and have its announcer mint
        // instances (so they ARE in the announcer's live map). Scrub
        // must leave them alone.
        let catalogue = test_catalogue();
        let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
        let plugin = TestFactoryRespondent {
            name: "org.test.factory.live".into(),
            policy_choice: Some(RetractionPolicy::Dynamic),
        };
        engine
            .admit_factory_respondent(
                plugin,
                factory_respondent_manifest("org.test.factory.live"),
            )
            .await
            .unwrap();

        let announcer = engine
            .factory_announcers
            .lock()
            .unwrap()
            .get("org.test.factory.live")
            .cloned()
            .unwrap();
        announcer
            .announce(
                evo_plugin_sdk::contract::factory::InstanceAnnouncement::new(
                    "live-1",
                    vec![],
                ),
            )
            .await
            .unwrap();

        let pre = engine.state().subjects.snapshot_subjects().len();
        assert_eq!(pre, 1);

        let report = engine.scrub_factory_orphans().await;
        assert_eq!(report.forgotten, 0);
        assert_eq!(report.errored, 0);

        // Live instance survives.
        let post = engine.state().subjects.snapshot_subjects().len();
        assert_eq!(post, 1);
        assert_eq!(announcer.instance_count(), 1);
    }

    #[tokio::test]
    async fn scrub_factory_orphans_ignores_non_factory_subjects() {
        // A subject minted under any non-factory addressing scheme is
        // not in scope for the scrub: even if no announcer claims it,
        // the scrub leaves it alone.
        use evo_plugin_sdk::contract::ExternalAddressing;
        use evo_plugin_sdk::contract::SubjectAnnouncement;

        let catalogue = test_catalogue();
        let engine = test_engine_from_catalogue(Arc::clone(&catalogue));

        let announcement = SubjectAnnouncement {
            subject_type: "test.ping".into(),
            addressings: vec![ExternalAddressing::new(
                "mpd-path",
                "/library/track-1.flac",
            )],
            claims: Vec::new(),
            state: serde_json::Value::Null,
            announced_at: std::time::SystemTime::now(),
        };
        engine
            .state()
            .subjects
            .announce(&announcement, "org.test.singleton")
            .unwrap();

        let report = engine.scrub_factory_orphans().await;
        assert_eq!(report.forgotten, 0);
        assert_eq!(report.errored, 0);

        // Subject still present.
        assert_eq!(engine.state().subjects.snapshot_subjects().len(), 1);
    }

    #[tokio::test]
    async fn admit_factory_respondent_supports_all_retraction_policies() {
        for (policy, suffix) in [
            (RetractionPolicy::Dynamic, "dynamic"),
            (RetractionPolicy::StartupOnly, "startup-only"),
            (RetractionPolicy::ShutdownOnly, "shutdown-only"),
        ] {
            let catalogue = test_catalogue();
            let mut engine = test_engine_from_catalogue(Arc::clone(&catalogue));
            let name = format!("org.test.factory.{suffix}");
            let plugin = TestFactoryRespondent {
                name: name.clone(),
                policy_choice: Some(policy),
            };
            engine
                .admit_factory_respondent(
                    plugin,
                    factory_respondent_manifest(&name),
                )
                .await
                .unwrap_or_else(|e| {
                    panic!("factory admits with policy {policy:?}: {e}")
                });
            assert_eq!(engine.len(), 1);
        }
    }

    /// URI-scheme admission tests. Verifies the AdmissionEngine
    /// registers schemes declared in `[capabilities.source]` at
    /// admit time, refuses conflicting registrations from a second
    /// plugin, and unregisters schemes on disable / shutdown so
    /// the operator can re-admit a successor without manual
    /// cleanup.
    mod uri_scheme_admission_tests {
        use super::*;
        use crate::queue::UriSchemeRegistry;

        /// Catalogue with two source shelves so two concurrent
        /// source plugins can land without colliding on shelves.
        fn source_catalogue() -> Arc<Catalogue> {
            Arc::new(
                Catalogue::from_toml(
                    r#"
schema_version = 1

[[racks]]
name = "test"
family = "domain"
charter = "test rack"

[[racks.shelves]]
name = "ping"
shape = 1
description = "test shelf"

[[racks.shelves]]
name = "source-a"
shape = 1
description = "test source plugin shelf A"

[[racks.shelves]]
name = "source-b"
shape = 1
description = "test source plugin shelf B"

[[subjects]]
name = "track"
"#,
                )
                .unwrap(),
            )
        }

        fn engine_with_uri_schemes(
            catalogue: Arc<Catalogue>,
        ) -> (AdmissionEngine, Arc<UriSchemeRegistry>) {
            let state = StewardState::for_tests_with_catalogue(catalogue);
            let registry = Arc::new(UriSchemeRegistry::new(Arc::clone(
                &state.persistence,
            )));
            let engine = AdmissionEngine::new(
                state,
                test_plugin_data_root(),
                std::path::PathBuf::new(),
                None,
                PluginsSecurityConfig::default(),
            )
            .with_uri_schemes(Arc::clone(&registry));
            (engine, registry)
        }

        fn source_manifest(
            name: &str,
            shelf_leaf: &str,
            schemes: &[&str],
        ) -> Manifest {
            let schemes_toml = schemes
                .iter()
                .map(|s| format!("{s:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            let toml = format!(
                r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "test.{shelf_leaf}"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["play_now"]
response_budget_ms = 1000

[capabilities.source]
uri_schemes = [{schemes_toml}]
"#
            );
            Manifest::from_toml(&toml).unwrap()
        }

        #[tokio::test]
        async fn admission_registers_uri_schemes() {
            let (mut engine, registry) =
                engine_with_uri_schemes(source_catalogue());
            let plugin = TestRespondent {
                name: "org.test.source.a".into(),
                runtime_request_types: Some(vec!["play_now".into()]),
                ..Default::default()
            };
            let manifest = source_manifest(
                "org.test.source.a",
                "source-a",
                &["evo-test-a"],
            );
            engine
                .admit_singleton_respondent(plugin, manifest)
                .await
                .expect("source plugin admits cleanly");
            let row = registry
                .lookup("evo-test-a")
                .await
                .expect("registry lookup succeeds")
                .expect("scheme is registered");
            assert_eq!(row.source_plugin, "org.test.source.a");
        }

        #[tokio::test]
        async fn admission_refuses_conflicting_uri_scheme() {
            let (mut engine, registry) =
                engine_with_uri_schemes(source_catalogue());
            let plugin_a = TestRespondent {
                name: "org.test.source.a".into(),
                runtime_request_types: Some(vec!["play_now".into()]),
                ..Default::default()
            };
            engine
                .admit_singleton_respondent(
                    plugin_a,
                    source_manifest(
                        "org.test.source.a",
                        "source-a",
                        &["evo-shared"],
                    ),
                )
                .await
                .expect("first source plugin admits");

            let plugin_b = TestRespondent {
                name: "org.test.source.b".into(),
                runtime_request_types: Some(vec!["play_now".into()]),
                ..Default::default()
            };
            let err = engine
                .admit_singleton_respondent(
                    plugin_b,
                    source_manifest(
                        "org.test.source.b",
                        "source-b",
                        &["evo-shared"],
                    ),
                )
                .await
                .expect_err(
                    "second plugin claiming the same scheme must be refused",
                );
            assert!(
                matches!(err, StewardError::Admission(_)),
                "expected Admission error, got {err:?}",
            );

            // The scheme is still owned by the first plugin; the
            // refused admission did not change the registry.
            let row = registry
                .lookup("evo-shared")
                .await
                .expect("registry lookup succeeds")
                .expect("scheme still owned");
            assert_eq!(row.source_plugin, "org.test.source.a");
            // The first plugin is still admitted; the second is
            // not.
            assert_eq!(engine.len(), 1);
        }

        #[tokio::test]
        async fn admission_partial_failure_rolls_back_uri_schemes() {
            // First plugin owns scheme-1. Second plugin's manifest
            // declares schemes-1-and-2; admission tries to register
            // both, hits the conflict on scheme-1, and must roll
            // back so scheme-2 is not silently leaked.
            let (mut engine, registry) =
                engine_with_uri_schemes(source_catalogue());
            engine
                .admit_singleton_respondent(
                    TestRespondent {
                        name: "org.test.source.a".into(),
                        runtime_request_types: Some(vec!["play_now".into()]),
                        ..Default::default()
                    },
                    source_manifest(
                        "org.test.source.a",
                        "source-a",
                        &["scheme-1"],
                    ),
                )
                .await
                .expect("first plugin admits");

            // Second plugin tries to register both scheme-1
            // (conflict) and scheme-2 (free).
            let plugin_b = TestRespondent {
                name: "org.test.source.b".into(),
                runtime_request_types: Some(vec!["play_now".into()]),
                ..Default::default()
            };
            let err = engine
                .admit_singleton_respondent(
                    plugin_b,
                    source_manifest(
                        "org.test.source.b",
                        "source-b",
                        &["scheme-2", "scheme-1"],
                    ),
                )
                .await
                .expect_err("conflict on scheme-1 must refuse admission");
            assert!(matches!(err, StewardError::Admission(_)));
            // scheme-2 must NOT be left registered to plugin.b: the
            // partial registration is rolled back.
            assert!(
                registry
                    .lookup("scheme-2")
                    .await
                    .expect("registry lookup succeeds")
                    .is_none(),
                "scheme-2 must be released after rollback",
            );
        }

        #[tokio::test]
        async fn disable_plugin_releases_uri_schemes() {
            let (engine, registry) =
                engine_with_uri_schemes(source_catalogue());
            let mut engine = engine;
            engine
                .admit_singleton_respondent(
                    TestRespondent {
                        name: "org.test.source.a".into(),
                        runtime_request_types: Some(vec!["play_now".into()]),
                        ..Default::default()
                    },
                    source_manifest(
                        "org.test.source.a",
                        "source-a",
                        &["evo-released"],
                    ),
                )
                .await
                .expect("admit");
            assert!(
                registry
                    .lookup("evo-released")
                    .await
                    .expect("registry lookup succeeds")
                    .is_some(),
                "scheme registered after admit",
            );

            engine
                .disable_plugin(
                    "org.test.source.a",
                    Some("test disable".into()),
                )
                .await
                .expect("disable");
            assert!(
                registry
                    .lookup("evo-released")
                    .await
                    .expect("registry lookup succeeds")
                    .is_none(),
                "scheme released after disable",
            );
        }

        #[tokio::test]
        async fn shutdown_releases_uri_schemes() {
            let (engine, registry) =
                engine_with_uri_schemes(source_catalogue());
            let mut engine = engine;
            engine
                .admit_singleton_respondent(
                    TestRespondent {
                        name: "org.test.source.a".into(),
                        runtime_request_types: Some(vec!["play_now".into()]),
                        ..Default::default()
                    },
                    source_manifest(
                        "org.test.source.a",
                        "source-a",
                        &["evo-shut"],
                    ),
                )
                .await
                .expect("admit");
            engine.shutdown().await.expect("shutdown");
            assert!(
                registry
                    .lookup("evo-shut")
                    .await
                    .expect("registry lookup succeeds")
                    .is_none(),
                "scheme released after shutdown",
            );
        }

        #[tokio::test]
        async fn admission_without_uri_scheme_registry_silently_skips() {
            // Engines built without `with_uri_schemes` must still
            // admit source-shaped plugins (the manifest is valid;
            // the framework just has no central registry to write
            // into). This is the test-harness path, used by
            // hundreds of existing engine tests that do not need
            // URI-scheme dispatch.
            let state =
                StewardState::for_tests_with_catalogue(source_catalogue());
            let mut engine = AdmissionEngine::new(
                state,
                test_plugin_data_root(),
                std::path::PathBuf::new(),
                None,
                PluginsSecurityConfig::default(),
            );
            // No `.with_uri_schemes(...)` here.
            engine
                .admit_singleton_respondent(
                    TestRespondent {
                        name: "org.test.source.a".into(),
                        runtime_request_types: Some(vec!["play_now".into()]),
                        ..Default::default()
                    },
                    source_manifest(
                        "org.test.source.a",
                        "source-a",
                        &["evo-noregistry"],
                    ),
                )
                .await
                .expect("admit succeeds without registry handle");
            assert_eq!(engine.len(), 1);
        }
    }

    mod ui_shelf_change_classification {
        use super::super::compute_ui_shelf_changes;
        use crate::happenings::{Happening, UiShelfChange};
        use evo_plugin_sdk::ui::UiStocking;

        fn stocking(shelf: &str) -> UiStocking {
            UiStocking {
                ui_shelf: shelf.to_string(),
                widget: "audio.browse.tree.entry".to_string(),
                size: evo_plugin_sdk::ui::UiSize::Third,
                mode: None,
                responsive: std::collections::BTreeMap::new(),
                parameters: std::collections::BTreeMap::new(),
                schema_version: 1,
                priority: None,
            }
        }

        fn shelves_and_changes(
            happenings: &[Happening],
        ) -> Vec<(String, UiShelfChange, u32)> {
            happenings
                .iter()
                .map(|h| match h {
                    Happening::UiShelfChanged {
                        shelf,
                        change,
                        stockings_after,
                        ..
                    } => (shelf.clone(), *change, *stockings_after),
                    _ => panic!("expected UiShelfChanged, got {h:?}"),
                })
                .collect()
        }

        #[test]
        fn empty_before_and_after_emits_nothing() {
            let changes = compute_ui_shelf_changes("p", &[], &[]);
            assert!(changes.is_empty());
        }

        #[test]
        fn first_admission_emits_stocked() {
            let after = vec![stocking("library.sources")];
            let changes = compute_ui_shelf_changes("p", &[], &after);
            assert_eq!(
                shelves_and_changes(&changes),
                vec![(
                    "library.sources".to_string(),
                    UiShelfChange::Stocked,
                    1
                )]
            );
        }

        #[test]
        fn full_withdrawal_emits_withdrawn() {
            let before = vec![stocking("library.sources")];
            let changes = compute_ui_shelf_changes("p", &before, &[]);
            assert_eq!(
                shelves_and_changes(&changes),
                vec![(
                    "library.sources".to_string(),
                    UiShelfChange::Withdrawn,
                    0
                )]
            );
        }

        #[test]
        fn re_admission_with_same_shelf_emits_restocked() {
            let before = vec![stocking("library.sources")];
            let after = vec![stocking("library.sources")];
            let changes = compute_ui_shelf_changes("p", &before, &after);
            assert_eq!(
                shelves_and_changes(&changes),
                vec![(
                    "library.sources".to_string(),
                    UiShelfChange::Restocked,
                    1
                )]
            );
        }

        #[test]
        fn count_change_is_reflected_in_stockings_after() {
            // Plugin had 1 stocking on library.sources, now
            // has 3.
            let before = vec![stocking("library.sources")];
            let after = vec![
                stocking("library.sources"),
                stocking("library.sources"),
                stocking("library.sources"),
            ];
            let changes = compute_ui_shelf_changes("p", &before, &after);
            assert_eq!(changes.len(), 1);
            match &changes[0] {
                Happening::UiShelfChanged {
                    change,
                    stockings_after,
                    ..
                } => {
                    assert_eq!(*change, UiShelfChange::Restocked);
                    assert_eq!(*stockings_after, 3);
                }
                other => panic!("expected UiShelfChanged, got {other:?}"),
            }
        }

        #[test]
        fn mixed_shelf_transitions_emit_one_event_per_shelf() {
            // Plugin previously stocked A; now stocks B and
            // also still stocks A. Expect: A=Restocked,
            // B=Stocked.
            let before = vec![stocking("a.shelf")];
            let after = vec![stocking("a.shelf"), stocking("b.shelf")];
            let changes = compute_ui_shelf_changes("p", &before, &after);
            let mut sc = shelves_and_changes(&changes);
            sc.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                sc,
                vec![
                    ("a.shelf".to_string(), UiShelfChange::Restocked, 1),
                    ("b.shelf".to_string(), UiShelfChange::Stocked, 1),
                ]
            );
        }

        #[test]
        fn shelf_drop_emits_withdrawn_for_that_shelf_only() {
            // Plugin had stockings on A and B; now stocks
            // only B. Expect: A=Withdrawn, B=Restocked.
            let before = vec![stocking("a.shelf"), stocking("b.shelf")];
            let after = vec![stocking("b.shelf")];
            let changes = compute_ui_shelf_changes("p", &before, &after);
            let mut sc = shelves_and_changes(&changes);
            sc.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                sc,
                vec![
                    ("a.shelf".to_string(), UiShelfChange::Withdrawn, 0),
                    ("b.shelf".to_string(), UiShelfChange::Restocked, 1),
                ]
            );
        }

        #[test]
        fn carries_plugin_name_on_every_event() {
            let after = vec![stocking("a.shelf")];
            let changes = compute_ui_shelf_changes("com.example", &[], &after);
            match &changes[0] {
                Happening::UiShelfChanged { plugin, .. } => {
                    assert_eq!(plugin, "com.example");
                }
                other => panic!("expected UiShelfChanged, got {other:?}"),
            }
        }
    }
}
