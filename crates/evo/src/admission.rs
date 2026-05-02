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
mod spawn;
mod validation;

pub use erasure::{
    ErasedRespondent, ErasedWarden, RespondentAdapter, WardenAdapter,
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
use crate::relations::RelationGraph;
use crate::router::{EnforcementPolicy, PluginEntry, PluginRouter};
use crate::state::StewardState;
use crate::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::factory::Factory;
use evo_plugin_sdk::contract::{
    HealthReport, InstanceAnnouncer, LoadContext, Respondent, Warden,
};
use evo_plugin_sdk::manifest::{
    InstanceShape, InteractionShape, TransportKind,
};
use evo_plugin_sdk::Manifest;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

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
}

impl std::fmt::Debug for AdmissionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionEngine")
            .field("plugin_count", &self.router.len())
            .field("admission_order", &self.router.admission_order())
            .finish()
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

    /// Borrow a handle to the catalogue this engine validates
    /// admissions against.
    pub fn catalogue(&self) -> Arc<Catalogue> {
        Arc::clone(&self.state.catalogue)
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
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = self
            .state
            .catalogue
            .find_shelf(&shelf_qualified)
            .ok_or_else(|| {
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

        if self.router.contains_shelf(&shelf_qualified) {
            return Err(StewardError::Admission(format!(
                "{}: shelf {} already occupied",
                manifest.plugin.name, shelf_qualified
            )));
        }

        if manifest.kind.interaction != InteractionShape::Respondent {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_respondent requires kind.interaction \
                 = 'respondent', manifest declares {:?}",
                manifest.plugin.name, manifest.kind.interaction
            )));
        }
        if manifest.kind.instance != InstanceShape::Singleton {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_respondent requires kind.instance \
                 = 'singleton', manifest declares {:?} \
                 (use admit_factory_respondent for factories)",
                manifest.plugin.name, manifest.kind.instance
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

        let ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            Arc::clone(&self.state.catalogue),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
        )?;
        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

        let kind_name = handle.kind_name();
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            trust_class = ?manifest.trust.class,
            "plugin admitted"
        );

        let entry = Arc::new(PluginEntry::new_with_policy(
            manifest.plugin.name.clone(),
            shelf_qualified.clone(),
            handle,
            EnforcementPolicy::from_manifest(&manifest),
        ));
        self.router.insert(entry)?;

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
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = self
            .state
            .catalogue
            .find_shelf(&shelf_qualified)
            .ok_or_else(|| {
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

        if self.router.contains_shelf(&shelf_qualified) {
            return Err(StewardError::Admission(format!(
                "{}: shelf {} already occupied",
                manifest.plugin.name, shelf_qualified
            )));
        }

        if manifest.kind.interaction != InteractionShape::Warden {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_warden requires kind.interaction \
                 = 'warden', manifest declares {:?}",
                manifest.plugin.name, manifest.kind.interaction
            )));
        }
        if manifest.kind.instance != InstanceShape::Singleton {
            return Err(StewardError::Admission(format!(
                "{}: admit_singleton_warden requires kind.instance \
                 = 'singleton', manifest declares {:?} \
                 (use admit_factory_warden for factories)",
                manifest.plugin.name, manifest.kind.instance
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

        let ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            Arc::clone(&self.state.catalogue),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
        )?;
        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

        let kind_name = handle.kind_name();
        tracing::info!(
            plugin = %manifest.plugin.name,
            shelf = %shelf_qualified,
            kind = %kind_name,
            trust_class = ?manifest.trust.class,
            "plugin admitted"
        );

        let entry = Arc::new(PluginEntry::new_with_policy(
            manifest.plugin.name.clone(),
            shelf_qualified.clone(),
            handle,
            EnforcementPolicy::from_manifest(&manifest),
        ));
        self.router.insert(entry)?;

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
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = self
            .state
            .catalogue
            .find_shelf(&shelf_qualified)
            .ok_or_else(|| {
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

        if self.router.contains_shelf(&shelf_qualified) {
            return Err(StewardError::Admission(format!(
                "{}: shelf {} already occupied",
                manifest.plugin.name, shelf_qualified
            )));
        }

        if manifest.kind.interaction != InteractionShape::Respondent {
            return Err(StewardError::Admission(format!(
                "{}: admit_factory_respondent requires kind.interaction \
                 = 'respondent', manifest declares {:?}",
                manifest.plugin.name, manifest.kind.interaction
            )));
        }
        if manifest.kind.instance != InstanceShape::Factory {
            return Err(StewardError::Admission(format!(
                "{}: admit_factory_respondent requires kind.instance \
                 = 'factory', manifest declares {:?} \
                 (use admit_singleton_respondent for singletons)",
                manifest.plugin.name, manifest.kind.instance
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

        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            Arc::clone(&self.state.catalogue),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
        )?;
        ctx.instance_announcer =
            Arc::clone(&announcer) as Arc<dyn InstanceAnnouncer>;

        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

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

        let entry = Arc::new(PluginEntry::new_with_policy(
            manifest.plugin.name.clone(),
            shelf_qualified.clone(),
            handle,
            EnforcementPolicy::from_manifest(&manifest),
        ));
        self.router.insert(entry)?;

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
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = self
            .state
            .catalogue
            .find_shelf(&shelf_qualified)
            .ok_or_else(|| {
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

        if self.router.contains_shelf(&shelf_qualified) {
            return Err(StewardError::Admission(format!(
                "{}: shelf {} already occupied",
                manifest.plugin.name, shelf_qualified
            )));
        }

        if manifest.kind.interaction != InteractionShape::Warden {
            return Err(StewardError::Admission(format!(
                "{}: admit_factory_warden requires kind.interaction \
                 = 'warden', manifest declares {:?}",
                manifest.plugin.name, manifest.kind.interaction
            )));
        }
        if manifest.kind.instance != InstanceShape::Factory {
            return Err(StewardError::Admission(format!(
                "{}: admit_factory_warden requires kind.instance \
                 = 'factory', manifest declares {:?} \
                 (use admit_singleton_warden for singletons)",
                manifest.plugin.name, manifest.kind.instance
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

        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            Arc::clone(&self.state.catalogue),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
        )?;
        ctx.instance_announcer =
            Arc::clone(&announcer) as Arc<dyn InstanceAnnouncer>;

        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

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

        let entry = Arc::new(PluginEntry::new_with_policy(
            manifest.plugin.name.clone(),
            shelf_qualified.clone(),
            handle,
            EnforcementPolicy::from_manifest(&manifest),
        ));
        self.router.insert(entry)?;

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
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = self
            .state
            .catalogue
            .find_shelf(&shelf_qualified)
            .ok_or_else(|| {
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

        if self.router.contains_shelf(&shelf_qualified) {
            return Err(StewardError::Admission(format!(
                "{}: shelf {} already occupied",
                manifest.plugin.name, shelf_qualified
            )));
        }

        if manifest.kind.interaction != InteractionShape::Respondent {
            return Err(StewardError::Admission(format!(
                "{}: admit_out_of_process_respondent requires kind.interaction \
                 = 'respondent', manifest declares {:?}",
                manifest.plugin.name, manifest.kind.interaction
            )));
        }

        // Connect and eagerly describe. If either the connection or
        // the initial describe fails we return here with no partial
        // state.
        let respondent = crate::wire_client::WireRespondent::connect(
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

        let mut handle = AdmittedHandle::Respondent(Box::new(respondent));

        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            Arc::clone(&self.state.catalogue),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
        )?;

        // For factory plugins, swap the default LoggingInstanceAnnouncer
        // for a real RegistryInstanceAnnouncer that mints subjects and
        // emits happenings. Out-of-process factories default to
        // RetractionPolicy::Dynamic; a future enhancement carries the
        // declared policy across the wire (or through a manifest field)
        // so StartupOnly / ShutdownOnly are operator-controllable for
        // OOP factories. The wiring layer's WireRespondent::load reads
        // ctx.instance_announcer when constructing the EventSink.
        let factory_announcer =
            if manifest.kind.instance == InstanceShape::Factory {
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

        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

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

        let entry = Arc::new(PluginEntry::new_with_policy(
            manifest.plugin.name.clone(),
            shelf_qualified.clone(),
            handle,
            EnforcementPolicy::from_manifest(&manifest),
        ));
        self.router.insert(entry)?;

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
        validation::check_manifest_prerequisites(&manifest)?;
        validation::check_admin_trust(&manifest)?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = self
            .state
            .catalogue
            .find_shelf(&shelf_qualified)
            .ok_or_else(|| {
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

        if self.router.contains_shelf(&shelf_qualified) {
            return Err(StewardError::Admission(format!(
                "{}: shelf {} already occupied",
                manifest.plugin.name, shelf_qualified
            )));
        }

        if manifest.kind.interaction != InteractionShape::Warden {
            return Err(StewardError::Admission(format!(
                "{}: admit_out_of_process_warden requires kind.interaction \
                 = 'warden', manifest declares {:?}",
                manifest.plugin.name, manifest.kind.interaction
            )));
        }

        // Connect and eagerly describe. Hands the shared custody
        // ledger and happenings bus to WireWarden so its load()
        // installs a LedgerCustodyStateReporter in the event sink
        // that updates both.
        let warden = crate::wire_client::WireWarden::connect(
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

        let mut handle = AdmittedHandle::Warden(Box::new(warden));

        let mut ctx = build_load_context(
            &self.plugin_data_root,
            &self.plugins_config_dir,
            &manifest,
            Arc::clone(&self.state.subjects),
            Arc::clone(&self.state.relations),
            Arc::clone(&self.state.catalogue),
            Arc::clone(&self.state.bus),
            Arc::clone(&self.state.admin),
            Arc::clone(&self.router),
            Arc::clone(&self.state.persistence),
            Arc::clone(&self.state.conflict_index),
        )?;

        // For factory plugins, swap the default LoggingInstanceAnnouncer
        // for a real RegistryInstanceAnnouncer. See the parallel block
        // in admit_out_of_process_respondent for the rationale +
        // RetractionPolicy::Dynamic default for OOP factories.
        let factory_announcer =
            if manifest.kind.instance == InstanceShape::Factory {
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

        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

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

        let entry = Arc::new(PluginEntry::new_with_policy(
            manifest.plugin.name.clone(),
            shelf_qualified.clone(),
            handle,
            EnforcementPolicy::from_manifest(&manifest),
        ));
        self.router.insert(entry)?;

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
        validation::check_manifest_prerequisites(&manifest)?;

        // This entry point only handles out-of-process plugins. The
        // in-process path requires a concrete Rust type that cannot be
        // materialised from a manifest alone.
        if manifest.transport.kind != TransportKind::OutOfProcess {
            return Err(StewardError::Admission(format!(
                "{}: admit_out_of_process_from_directory requires \
                 transport.type = 'out-of-process', manifest declares {:?}",
                manifest.plugin.name, manifest.transport.kind
            )));
        }

        // Resolve the executable path. Relative paths in `exec` are
        // relative to the plugin directory; absolute paths are used
        // verbatim. Absolute paths bypass the plugin-bundle convention
        // and are intended for test environments; production plugins
        // should always use relative paths.
        let exec_path = {
            let raw = Path::new(&manifest.transport.exec);
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
        let admission_result = match manifest.kind.interaction {
            InteractionShape::Respondent => {
                self.admit_out_of_process_respondent(
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
        // record on the router so shutdown can reap it.
        let shelf_qualified = manifest.target.shelf.clone();
        if !self.router.attach_child(&shelf_qualified, child).await {
            // Should be impossible: admit_out_of_process_respondent
            // just inserted this record. If it is missing, something
            // is deeply wrong; the child cannot be reattached so the
            // router has effectively lost it. Log loudly. The child
            // would have been moved into attach_child if the entry
            // existed, so reaching here means the entry vanished
            // before we could attach.
            tracing::error!(
                plugin = %manifest.plugin.name,
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

        Ok(())
    }

    /// Hot-reload a single admitted plugin per its declared
    /// `lifecycle.hot_reload` policy.
    ///
    /// Three policy outcomes:
    ///
    /// - `HotReloadPolicy::None` — refused with a structured
    ///   error pointing the operator at explicit unload + admit.
    ///   The plugin author opted out of hot reload; respecting
    ///   that choice is correct behaviour.
    /// - `HotReloadPolicy::Restart` — the steward unloads the
    ///   plugin and re-admits it from the same source. Today the
    ///   only reachable origin is
    ///   [`Self::admit_out_of_process_from_directory`]; plugins
    ///   admitted via the typed in-process or programmatic OOP
    ///   `admit_*` entry points have no recorded directory and
    ///   are refused with a structured error pointing at the
    ///   distribution-layer admit code.
    /// - `HotReloadPolicy::Live` — refused. The wire-side
    ///   `reload_in_place` verb is documented but not yet on the
    ///   wire; the SDK host returns `ReportError::Invalid` for
    ///   it. Live reload becomes operative when that verb ships.
    ///
    /// `runtime_dir` mirrors the parameter on
    /// [`Self::admit_out_of_process_from_directory`]; it must
    /// exist and be writable by the steward.
    ///
    /// On success, the plugin is unloaded and re-admitted; the
    /// shelf may briefly hold no plugin during the gap. Callers
    /// observing the happenings stream see one `unload` event
    /// followed by a fresh admit (and any factory-instance
    /// re-announcements) per the standard admission paths.
    pub async fn reload_plugin(
        &mut self,
        plugin_name: &str,
        runtime_dir: &std::path::Path,
    ) -> Result<(), StewardError> {
        // Look up the plugin by canonical name. The router is
        // keyed by shelf; the lookup walks the admission_order.
        let entry = self.router.lookup_by_name(plugin_name).ok_or_else(|| {
            StewardError::Dispatch(format!(
                "no plugin admitted with name {plugin_name}"
            ))
        })?;

        // Resolve the recorded origin. Programmatic plugins
        // (in-process, typed OOP) do not have one and cannot be
        // reloaded by the framework alone.
        let plugin_dir = self
            .plugin_origins
            .lock()
            .expect("plugin_origins mutex poisoned")
            .get(plugin_name)
            .cloned();

        // Read the plugin's manifest to learn its hot_reload
        // policy. The manifest is the source of truth; the
        // EnforcementPolicy stored on the entry does not carry
        // the lifecycle field today.
        let manifest_path = match plugin_dir.as_ref() {
            Some(dir) => dir.join("manifest.toml"),
            None => {
                return Err(StewardError::Dispatch(format!(
                    "{plugin_name}: cannot reload; the plugin was admitted \
                     programmatically (no source directory recorded). The \
                     distribution's admit code must perform unload + admit \
                     against a fresh handle."
                )));
            }
        };
        let manifest_text =
            std::fs::read_to_string(&manifest_path).map_err(|e| {
                StewardError::io(
                    format!(
                        "reading {} for reload",
                        manifest_path.display()
                    ),
                    e,
                )
            })?;
        let manifest = Manifest::from_toml(&manifest_text)?;

        match manifest.lifecycle.hot_reload {
            evo_plugin_sdk::manifest::HotReloadPolicy::None => {
                Err(StewardError::Dispatch(format!(
                    "{plugin_name}: lifecycle.hot_reload = none; the plugin \
                     opted out of hot reload. Use unload + admit instead, \
                     or update the manifest to declare a different policy."
                )))
            }
            evo_plugin_sdk::manifest::HotReloadPolicy::Live => {
                Err(StewardError::Dispatch(format!(
                    "{plugin_name}: lifecycle.hot_reload = live requires the \
                     reload_in_place wire verb, which is documented but \
                     deferred until a future release. Declare \
                     hot_reload = restart for the framework's current \
                     reload behaviour."
                )))
            }
            evo_plugin_sdk::manifest::HotReloadPolicy::Restart => {
                let plugin_dir = plugin_dir.expect("checked above");
                let shelf = entry.shelf.clone();

                // Evict from the routing table first so the shelf
                // is free for the re-admit. Concurrent dispatches
                // see a structured "no plugin on shelf" until the
                // re-admit completes.
                let removed = self
                    .router
                    .remove(&shelf)
                    .ok_or_else(|| {
                        StewardError::Dispatch(format!(
                            "{plugin_name}: router lost the entry between \
                             lookup and remove"
                        ))
                    })?;
                // Drop the shared lookup reference so the only
                // remaining strong owner of the Arc is `removed`;
                // unload_one_plugin needs to take the handle out
                // of the entry and that requires sole ownership
                // of the contents (the AsyncMutex serialises
                // taking the inner option, so multiple Arcs are
                // not actually a soundness problem, but the
                // pattern matches the drain path).
                drop(entry);

                // Drop the recorded origin BEFORE the re-admit
                // path inserts a fresh one; otherwise a concurrent
                // observer could see two distinct origins for the
                // same plugin name during the gap.
                self.plugin_origins
                    .lock()
                    .expect("plugin_origins mutex poisoned")
                    .remove(plugin_name);

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

                // Re-admit from the same directory.
                self.admit_out_of_process_from_directory(
                    &plugin_dir,
                    runtime_dir,
                )
                .await
            }
        }
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
        let entries = self.router.drain_in_reverse_admission_order();
        let plugins_total = entries.len();

        let (plugins_unloaded_cleanly, plugins_killed_after_deadline) =
            parallel_unload_with_deadline(entries, config.global_deadline)
                .await;

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
                Arc::clone(&self.state.catalogue),
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
                // for visibility without attempting release.
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
            set.abort_all();
            while (set.join_next().await).is_some() {}
            for rec in ledger.list_active() {
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
                    "custody release during shutdown failed"
                );
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
) -> Result<LoadContext, StewardError> {
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
    // Both admin announcers carry an `Arc<PluginRouter>` so the
    // wiring layer can refuse `target_plugin` arguments that do
    // not name a currently-admitted plugin (typo guard) before
    // any storage-primitive call.
    let (subject_admin, relation_admin) = if manifest.capabilities.admin {
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
                    router,
                    manifest.plugin.name.clone(),
                )
                .with_persistence(Arc::clone(&persistence)),
            );
        (Some(subject_admin), Some(relation_admin))
    } else {
        (None, None)
    };

    let config = load_plugin_config(plugins_config_dir, &manifest.plugin.name)?;

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
        user_interaction_requester: Arc::new(
            LoggingUserInteractionRequester::new(manifest.plugin.name.clone()),
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
        subject_querier: Some(Arc::new(RegistrySubjectQuerier::new(registry))),
        subject_admin,
        relation_admin,
    })
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
    }

    impl Plugin for TestRespondent {
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
            &'a mut self,
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
    fn test_engine_from_catalogue(
        catalogue: Arc<Catalogue>,
    ) -> AdmissionEngine {
        let state = StewardState::for_tests_with_catalogue(catalogue);
        AdmissionEngine::new(
            state,
            PathBuf::from(crate::config::DEFAULT_PLUGIN_DATA_ROOT),
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
            PathBuf::from(crate::config::DEFAULT_PLUGIN_DATA_ROOT),
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
            PathBuf::from(crate::config::DEFAULT_PLUGIN_DATA_ROOT),
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
hot_reload = "restart"

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
        let runtime_dir = tempfile::tempdir().unwrap();
        let r = engine
            .reload_plugin("org.test.never-admitted", runtime_dir.path())
            .await;
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
        // path has no recorded source directory. The reload path
        // must refuse with a structured error pointing the
        // distribution at unload + admit, not silently succeed
        // with "nothing to do" or panic on the missing origin.
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

        let runtime_dir = tempfile::tempdir().unwrap();
        let r = engine.reload_plugin("org.test.ping", runtime_dir.path()).await;
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
            &'a mut self,
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
            &'a mut self,
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
            &'a mut self,
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
            &'a mut self,
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
            &'a mut self,
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
}
