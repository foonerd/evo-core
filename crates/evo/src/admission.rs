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

use crate::catalogue::Catalogue;
use crate::context::{
    LoggingInstanceAnnouncer, LoggingStateReporter,
    LoggingUserInteractionRequester, RegistryRelationAnnouncer,
    RegistrySubjectAnnouncer,
};
use crate::custody::{CustodyLedger, LedgerCustodyStateReporter};
use crate::error::StewardError;
use crate::happenings::{Happening, HappeningBus};
use crate::relations::RelationGraph;
use crate::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::{
    Assignment, CourseCorrection, CustodyHandle, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, Request, Respondent, Response,
    Warden,
};
use evo_plugin_sdk::manifest::{InteractionShape, TransportKind};
use evo_plugin_sdk::Manifest;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

/// Object-safe internal trait for admitted respondent plugins.
///
/// Public SDK traits use native async-in-trait; this internal trait uses
/// `Pin<Box<dyn Future>>` to be object-safe so the engine can store
/// heterogeneous plugins as `Box<dyn ErasedRespondent>`.
pub trait ErasedRespondent: Send + Sync {
    /// Dispatches to `Plugin::describe`.
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>>;

    /// Dispatches to `Plugin::load`.
    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;

    /// Dispatches to `Plugin::unload`.
    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>;

    /// Dispatches to `Plugin::health_check`.
    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>>;

    /// Dispatches to `Respondent::handle_request`.
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>;
}

/// Generic adapter: wraps any `T: Respondent + 'static` as an
/// [`ErasedRespondent`].
pub struct RespondentAdapter<T: Respondent + 'static> {
    inner: T,
}

impl<T: Respondent + 'static> RespondentAdapter<T> {
    /// Wrap a concrete respondent.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Unwrap the concrete respondent. Useful for tests.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: Respondent + 'static> ErasedRespondent for RespondentAdapter<T> {
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        Box::pin(Plugin::describe(&self.inner))
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>> {
        Box::pin(Plugin::load(&mut self.inner, ctx))
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>> {
        Box::pin(Plugin::unload(&mut self.inner))
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(Plugin::health_check(&self.inner))
    }

    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>
    {
        Box::pin(Respondent::handle_request(&mut self.inner, req))
    }
}

/// Object-safe internal trait for admitted warden plugins.
///
/// Parallels [`ErasedRespondent`]: same four core verbs from `Plugin`,
/// plus the three custody verbs from `Warden`. The engine stores
/// wardens as `Box<dyn ErasedWarden>` inside an [`AdmittedHandle`].
pub trait ErasedWarden: Send + Sync {
    /// Dispatches to `Plugin::describe`.
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>>;

    /// Dispatches to `Plugin::load`.
    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;

    /// Dispatches to `Plugin::unload`.
    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>;

    /// Dispatches to `Plugin::health_check`.
    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>>;

    /// Dispatches to `Warden::take_custody`.
    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> Pin<
        Box<dyn Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a>,
    >;

    /// Dispatches to `Warden::course_correct`.
    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;

    /// Dispatches to `Warden::release_custody`.
    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;
}

/// Generic adapter: wraps any `T: Warden + 'static` as an
/// [`ErasedWarden`]. Parallels [`RespondentAdapter`].
pub struct WardenAdapter<T: Warden + 'static> {
    inner: T,
}

impl<T: Warden + 'static> WardenAdapter<T> {
    /// Wrap a concrete warden.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Unwrap the concrete warden. Useful for tests.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: Warden + 'static> ErasedWarden for WardenAdapter<T> {
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        Box::pin(Plugin::describe(&self.inner))
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>> {
        Box::pin(Plugin::load(&mut self.inner, ctx))
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>> {
        Box::pin(Plugin::unload(&mut self.inner))
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(Plugin::health_check(&self.inner))
    }

    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> Pin<
        Box<dyn Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a>,
    > {
        Box::pin(Warden::take_custody(&mut self.inner, assignment))
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Warden::course_correct(&mut self.inner, handle, correction))
    }

    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Warden::release_custody(&mut self.inner, handle))
    }
}

/// A handle to an admitted plugin. Each admitted plugin is exactly one
/// of these variants, disjoint and decided at admission time by the
/// manifest's `kind.interaction` field.
///
/// All four core verbs (`describe`, `load`, `unload`, `health_check`)
/// are common to both variants and exposed via inherent methods that
/// dispatch through the enum. Kind-specific verbs are routed by the
/// engine's public methods, which match on the variant and return a
/// [`StewardError::Dispatch`] if the shelf's plugin kind does not
/// match the caller's request (e.g. a `handle_request` on a warden
/// shelf, or a `take_custody` on a respondent shelf).
pub enum AdmittedHandle {
    /// A respondent plugin: handles discrete request-response
    /// exchanges via [`ErasedRespondent::handle_request`].
    Respondent(Box<dyn ErasedRespondent>),
    /// A warden plugin: takes sustained custody via
    /// [`ErasedWarden::take_custody`], [`ErasedWarden::course_correct`],
    /// [`ErasedWarden::release_custody`].
    Warden(Box<dyn ErasedWarden>),
}

impl AdmittedHandle {
    /// Dispatch to the inner plugin's `describe`.
    async fn describe(&self) -> PluginDescription {
        match self {
            Self::Respondent(r) => r.describe().await,
            Self::Warden(w) => w.describe().await,
        }
    }

    /// Dispatch to the inner plugin's `load`.
    async fn load(
        &mut self,
        ctx: &LoadContext,
    ) -> Result<(), PluginError> {
        match self {
            Self::Respondent(r) => r.load(ctx).await,
            Self::Warden(w) => w.load(ctx).await,
        }
    }

    /// Dispatch to the inner plugin's `unload`.
    async fn unload(&mut self) -> Result<(), PluginError> {
        match self {
            Self::Respondent(r) => r.unload().await,
            Self::Warden(w) => w.unload().await,
        }
    }

    /// Dispatch to the inner plugin's `health_check`.
    async fn health_check(&self) -> HealthReport {
        match self {
            Self::Respondent(r) => r.health_check().await,
            Self::Warden(w) => w.health_check().await,
        }
    }

    /// Human-readable name of the interaction shape for diagnostics.
    fn kind_name(&self) -> &'static str {
        match self {
            Self::Respondent(_) => "respondent",
            Self::Warden(_) => "warden",
        }
    }
}

/// Internal record of one admitted plugin.
struct AdmittedPlugin {
    /// Canonical plugin name, per the manifest.
    name: String,
    /// Fully-qualified shelf this plugin occupies (`<rack>.<shelf>`).
    shelf: String,
    /// Type-erased handle for lifecycle and dispatch. Exactly one of
    /// the two variants per admission.
    handle: AdmittedHandle,
    /// Child process handle, if this plugin was spawned by the steward
    /// via [`AdmissionEngine::admit_out_of_process_from_directory`].
    ///
    /// The child is owned by the steward for its full lifetime. It is
    /// waited on (or killed after a timeout) during
    /// [`AdmissionEngine::shutdown`]. `kill_on_drop(true)` is set so
    /// even if the engine is dropped without a shutdown call the child
    /// is guaranteed to be reaped.
    ///
    /// `None` for in-process plugins and for out-of-process plugins
    /// admitted via [`AdmissionEngine::admit_out_of_process_respondent`]
    /// where the caller owns the child.
    child: Option<Child>,
}

impl std::fmt::Debug for AdmittedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedPlugin")
            .field("name", &self.name)
            .field("shelf", &self.shelf)
            .field("kind", &self.handle.kind_name())
            .field("child", &self.child.as_ref().map(|c| c.id()))
            .finish()
    }
}

/// The admission engine.
///
/// Holds admitted plugins keyed by fully-qualified shelf name. v0 permits
/// one plugin per shelf; additional admissions on the same shelf fail.
///
/// Owns the shared subject registry and relation graph used by admitted
/// plugins. Plugins receive `SubjectAnnouncer` and `RelationAnnouncer`
/// handles in their `LoadContext` that write to these stores tagged
/// with the plugin's name.
pub struct AdmissionEngine {
    /// Map of shelf name -> admitted plugin.
    by_shelf: HashMap<String, AdmittedPlugin>,
    /// Admission order, for reverse-order shutdown.
    admission_order: Vec<String>,
    /// Subject registry shared with all admitted plugins.
    registry: Arc<SubjectRegistry>,
    /// Relation graph shared with all admitted plugins.
    relation_graph: Arc<RelationGraph>,
    /// Custody ledger shared with the wire-warden adapter. Tracks
    /// every custody handed out by [`Self::take_custody`] and every
    /// state report emitted by the holding warden.
    custody_ledger: Arc<CustodyLedger>,
    /// Happenings bus. Receives [`Happening::CustodyTaken`] and
    /// [`Happening::CustodyReleased`] from this engine's custody
    /// verbs; receives [`Happening::CustodyStateReported`] from a
    /// future pass that routes the ledger-backed custody state
    /// reporter through the bus.
    happening_bus: Arc<HappeningBus>,
    /// Monotonic counter for correlation IDs on warden custody verbs
    /// (`take_custody`, `course_correct`). Each call allocates a fresh
    /// ID. Engine-local, not persistent across restarts.
    custody_cid_counter: Arc<AtomicU64>,
}

impl Default for AdmissionEngine {
    fn default() -> Self {
        Self {
            by_shelf: HashMap::new(),
            admission_order: Vec::new(),
            registry: Arc::new(SubjectRegistry::new()),
            relation_graph: Arc::new(RelationGraph::new()),
            custody_ledger: Arc::new(CustodyLedger::new()),
            happening_bus: Arc::new(HappeningBus::new()),
            custody_cid_counter: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl std::fmt::Debug for AdmissionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionEngine")
            .field("plugin_count", &self.by_shelf.len())
            .field("admission_order", &self.admission_order)
            .finish()
    }
}

impl AdmissionEngine {
    /// Construct an empty admission engine with a fresh subject registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct an admission engine sharing an existing subject
    /// registry. Useful for tests that want to pre-populate or inspect
    /// the registry from outside. A fresh relation graph is created.
    pub fn with_registry(registry: Arc<SubjectRegistry>) -> Self {
        Self {
            by_shelf: HashMap::new(),
            admission_order: Vec::new(),
            registry,
            relation_graph: Arc::new(RelationGraph::new()),
            custody_ledger: Arc::new(CustodyLedger::new()),
            happening_bus: Arc::new(HappeningBus::new()),
            custody_cid_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Construct an admission engine sharing existing subject registry
    /// and relation graph handles. Useful for tests that exercise both
    /// stores together, or for future dependency injection when the
    /// steward carries persisted state across restarts.
    pub fn with_registry_and_graph(
        registry: Arc<SubjectRegistry>,
        relation_graph: Arc<RelationGraph>,
    ) -> Self {
        Self {
            by_shelf: HashMap::new(),
            admission_order: Vec::new(),
            registry,
            relation_graph,
            custody_ledger: Arc::new(CustodyLedger::new()),
            happening_bus: Arc::new(HappeningBus::new()),
            custody_cid_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Construct an admission engine sharing existing subject
    /// registry, relation graph, and custody ledger handles.
    ///
    /// Parallel to [`Self::with_registry_and_graph`] for the case
    /// where the caller also wants to share a preexisting ledger.
    /// Useful for tests that assert on both engine dispatch and
    /// ledger state from outside the engine, and for future
    /// dependency injection when the steward carries persisted
    /// ledger state across restarts.
    pub fn with_registry_graph_and_ledger(
        registry: Arc<SubjectRegistry>,
        relation_graph: Arc<RelationGraph>,
        custody_ledger: Arc<CustodyLedger>,
    ) -> Self {
        Self {
            by_shelf: HashMap::new(),
            admission_order: Vec::new(),
            registry,
            relation_graph,
            custody_ledger,
            happening_bus: Arc::new(HappeningBus::new()),
            custody_cid_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Construct an admission engine sharing existing subject
    /// registry, relation graph, custody ledger, and happenings bus
    /// handles.
    ///
    /// Parallel to [`Self::with_registry_graph_and_ledger`] for the
    /// case where the caller also wants to share a preexisting bus.
    /// Tests that assert on happenings emitted from the engine's
    /// custody verbs use this constructor so the bus is reachable
    /// both from the engine and from a subscriber held by the test.
    pub fn with_registry_graph_ledger_and_bus(
        registry: Arc<SubjectRegistry>,
        relation_graph: Arc<RelationGraph>,
        custody_ledger: Arc<CustodyLedger>,
        happening_bus: Arc<HappeningBus>,
    ) -> Self {
        Self {
            by_shelf: HashMap::new(),
            admission_order: Vec::new(),
            registry,
            relation_graph,
            custody_ledger,
            happening_bus,
            custody_cid_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Borrow a handle to the subject registry used by this engine.
    pub fn registry(&self) -> Arc<SubjectRegistry> {
        Arc::clone(&self.registry)
    }

    /// Borrow a handle to the relation graph used by this engine.
    pub fn relation_graph(&self) -> Arc<RelationGraph> {
        Arc::clone(&self.relation_graph)
    }

    /// Borrow a handle to the custody ledger used by this engine.
    pub fn custody_ledger(&self) -> Arc<CustodyLedger> {
        Arc::clone(&self.custody_ledger)
    }

    /// Borrow a handle to the happenings bus used by this engine.
    pub fn happening_bus(&self) -> Arc<HappeningBus> {
        Arc::clone(&self.happening_bus)
    }

    /// Number of currently admitted plugins.
    pub fn len(&self) -> usize {
        self.by_shelf.len()
    }

    /// True if no plugins are admitted.
    pub fn is_empty(&self) -> bool {
        self.by_shelf.is_empty()
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
        catalogue: &Catalogue,
    ) -> Result<(), StewardError>
    where
        T: Respondent + 'static,
    {
        manifest.validate()?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: target shelf not in catalogue: {}",
                manifest.plugin.name, shelf_qualified
            ))
        })?;

        if shelf.shape != manifest.target.shape {
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape
            )));
        }

        if self.by_shelf.contains_key(&shelf_qualified) {
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
            &manifest,
            Arc::clone(&self.registry),
            Arc::clone(&self.relation_graph),
        );
        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

        let record = AdmittedPlugin {
            name: manifest.plugin.name.clone(),
            shelf: shelf_qualified.clone(),
            handle,
            child: None,
        };

        tracing::info!(
            plugin = %record.name,
            shelf = %record.shelf,
            kind = %record.handle.kind_name(),
            trust_class = ?manifest.trust.class,
            "plugin admitted"
        );

        self.by_shelf.insert(shelf_qualified.clone(), record);
        self.admission_order.push(shelf_qualified);

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
        catalogue: &Catalogue,
    ) -> Result<(), StewardError>
    where
        T: Warden + 'static,
    {
        manifest.validate()?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: target shelf not in catalogue: {}",
                manifest.plugin.name, shelf_qualified
            ))
        })?;

        if shelf.shape != manifest.target.shape {
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape
            )));
        }

        if self.by_shelf.contains_key(&shelf_qualified) {
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

        let mut handle = AdmittedHandle::Warden(Box::new(
            WardenAdapter::new(plugin),
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
            &manifest,
            Arc::clone(&self.registry),
            Arc::clone(&self.relation_graph),
        );
        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

        let record = AdmittedPlugin {
            name: manifest.plugin.name.clone(),
            shelf: shelf_qualified.clone(),
            handle,
            child: None,
        };

        tracing::info!(
            plugin = %record.name,
            shelf = %record.shelf,
            kind = %record.handle.kind_name(),
            trust_class = ?manifest.trust.class,
            "plugin admitted"
        );

        self.by_shelf.insert(shelf_qualified.clone(), record);
        self.admission_order.push(shelf_qualified);

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
        catalogue: &Catalogue,
        reader: R,
        writer: W,
    ) -> Result<(), StewardError>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        manifest.validate()?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: target shelf not in catalogue: {}",
                manifest.plugin.name, shelf_qualified
            ))
        })?;

        if shelf.shape != manifest.target.shape {
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape
            )));
        }

        if self.by_shelf.contains_key(&shelf_qualified) {
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

        let ctx = build_load_context(
            &manifest,
            Arc::clone(&self.registry),
            Arc::clone(&self.relation_graph),
        );
        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

        let record = AdmittedPlugin {
            name: manifest.plugin.name.clone(),
            shelf: shelf_qualified.clone(),
            handle,
            child: None,
        };

        tracing::info!(
            plugin = %record.name,
            shelf = %record.shelf,
            kind = %record.handle.kind_name(),
            trust_class = ?manifest.trust.class,
            transport = "wire",
            "plugin admitted"
        );

        self.by_shelf.insert(shelf_qualified.clone(), record);
        self.admission_order.push(shelf_qualified);

        Ok(())
    }

    /// Admit an out-of-process singleton warden over the wire
    /// protocol.
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
        catalogue: &Catalogue,
        reader: R,
        writer: W,
    ) -> Result<(), StewardError>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        manifest.validate()?;

        let shelf_qualified = manifest.target.shelf.clone();
        let shelf = catalogue.find_shelf(&shelf_qualified).ok_or_else(|| {
            StewardError::Admission(format!(
                "{}: target shelf not in catalogue: {}",
                manifest.plugin.name, shelf_qualified
            ))
        })?;

        if shelf.shape != manifest.target.shape {
            return Err(StewardError::Admission(format!(
                "{}: manifest targets shape {} but catalogue shelf {} is shape {}",
                manifest.plugin.name,
                manifest.target.shape,
                shelf_qualified,
                shelf.shape
            )));
        }

        if self.by_shelf.contains_key(&shelf_qualified) {
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
        // ledger to WireWarden so its load() installs a
        // LedgerCustodyStateReporter in the event sink.
        let warden = crate::wire_client::WireWarden::connect(
            reader,
            writer,
            manifest.plugin.name.clone(),
            Arc::clone(&self.custody_ledger),
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

        let ctx = build_load_context(
            &manifest,
            Arc::clone(&self.registry),
            Arc::clone(&self.relation_graph),
        );
        handle.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

        let record = AdmittedPlugin {
            name: manifest.plugin.name.clone(),
            shelf: shelf_qualified.clone(),
            handle,
            child: None,
        };

        tracing::info!(
            plugin = %record.name,
            shelf = %record.shelf,
            kind = %record.handle.kind_name(),
            trust_class = ?manifest.trust.class,
            transport = "wire",
            "plugin admitted"
        );

        self.by_shelf.insert(shelf_qualified.clone(), record);
        self.admission_order.push(shelf_qualified);

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
    /// 5. Spawns the plugin binary with that socket path as argv[1].
    ///    The child is spawned with `kill_on_drop(true)` so it cannot
    ///    outlive this engine even if shutdown is never called.
    /// 6. Polls for the socket to become connectable while checking
    ///    that the child has not already exited. Times out after
    ///    [`SOCKET_READY_TIMEOUT`].
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
        catalogue: &Catalogue,
    ) -> Result<(), StewardError> {
        // Read and validate the manifest from disk.
        let manifest_path = plugin_dir.join("manifest.toml");
        let manifest_text = std::fs::read_to_string(&manifest_path)
            .map_err(|e| {
                StewardError::io(
                    format!("reading {}", manifest_path.display()),
                    e,
                )
            })?;
        let manifest = Manifest::from_toml(&manifest_text)?;

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

        // Spawn the child.
        let mut child = Command::new(&exec_path)
            .arg(&socket_path)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                StewardError::io(
                    format!(
                        "spawning plugin binary {}",
                        exec_path.display()
                    ),
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
        let stream =
            match wait_for_socket_ready(&socket_path, &mut child).await {
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
                    catalogue,
                    reader,
                    writer,
                )
                .await
            }
            InteractionShape::Warden => {
                self.admit_out_of_process_warden(
                    manifest.clone(),
                    catalogue,
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
        // record so shutdown can reap it.
        let shelf_qualified = manifest.target.shelf.clone();
        if let Some(record) = self.by_shelf.get_mut(&shelf_qualified) {
            record.child = Some(child);
        } else {
            // Should be impossible: admit_out_of_process_respondent
            // just inserted this record. If it is missing, something
            // is deeply wrong; kill the child and log.
            tracing::error!(
                plugin = %manifest.plugin.name,
                shelf = %shelf_qualified,
                "admitted record missing after successful admission"
            );
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        Ok(())
    }

    /// Route a request to the plugin admitted on the given shelf.
    ///
    /// Returns [`StewardError::Dispatch`] if no plugin is admitted on
    /// that shelf, or if the plugin on that shelf is a warden (whose
    /// verbs are [`Self::take_custody`], [`Self::course_correct`],
    /// [`Self::release_custody`] - not `handle_request`).
    ///
    /// Returns the plugin's own error wrapped in
    /// [`StewardError::Plugin`] if the plugin's handler fails.
    pub async fn handle_request(
        &mut self,
        shelf: &str,
        request: Request,
    ) -> Result<Response, StewardError> {
        let plugin = self.by_shelf.get_mut(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;
        match &mut plugin.handle {
            AdmittedHandle::Respondent(r) => {
                r.handle_request(&request).await.map_err(Into::into)
            }
            AdmittedHandle::Warden(_) => Err(StewardError::Dispatch(format!(
                "handle_request on shelf {shelf}: plugin is a warden, \
                 not a respondent"
            ))),
        }
    }

    /// Deliver an assignment to the warden on the given shelf.
    ///
    /// Allocates a fresh correlation ID, constructs an [`Assignment`]
    /// carrying a [`LedgerCustodyStateReporter`] tagged with the
    /// warden's plugin name, and dispatches to the warden's
    /// `take_custody`. On success, records the custody in the
    /// engine's [`CustodyLedger`] keyed by `(plugin_name, handle.id)`
    /// with the full metadata (shelf, custody_type, handle), then
    /// emits a [`Happening::CustodyTaken`] on the engine's
    /// [`HappeningBus`].
    ///
    /// The happening is emitted AFTER the ledger write so any
    /// subscriber that reacts to the happening by querying the
    /// ledger will see the new record.
    ///
    /// If the warden emits an initial `ReportCustodyState` from
    /// within its own `take_custody` (typical for wire wardens),
    /// that state report may reach the ledger before this method
    /// records the custody metadata. The ledger's UPSERT semantics
    /// merge the two events into one record regardless of arrival
    /// order.
    ///
    /// Returns [`StewardError::Dispatch`] if no plugin is admitted
    /// on that shelf, or if the plugin there is a respondent (whose
    /// verb is [`Self::handle_request`] - not `take_custody`).
    ///
    /// Returns the plugin's own error wrapped in
    /// [`StewardError::Plugin`] if the warden rejects the
    /// assignment. On such failure neither the ledger nor the bus
    /// is written.
    pub async fn take_custody(
        &mut self,
        shelf: &str,
        custody_type: String,
        payload: Vec<u8>,
        deadline: Option<Instant>,
    ) -> Result<CustodyHandle, StewardError> {
        // Clone the ledger and bus Arcs up front for use after the
        // warden call. After this point we hold a mutable borrow of
        // self.by_shelf through `plugin`, so accessing
        // self.custody_ledger or self.happening_bus directly would
        // conflict.
        let ledger = Arc::clone(&self.custody_ledger);
        let bus = Arc::clone(&self.happening_bus);

        let plugin = self.by_shelf.get_mut(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;
        let warden = match &mut plugin.handle {
            AdmittedHandle::Warden(w) => w,
            AdmittedHandle::Respondent(_) => {
                return Err(StewardError::Dispatch(format!(
                    "take_custody on shelf {shelf}: plugin is a respondent, \
                     not a warden"
                )));
            }
        };

        // Clone fields we need after the await point. custody_type
        // is consumed by the Assignment below so clone it first.
        let plugin_name = plugin.name.clone();
        let shelf_qualified = plugin.shelf.clone();
        let custody_type_for_ledger = custody_type.clone();

        let correlation_id =
            self.custody_cid_counter.fetch_add(1, Ordering::Relaxed);
        let reporter: Arc<dyn evo_plugin_sdk::contract::CustodyStateReporter> =
            Arc::new(LedgerCustodyStateReporter::new(
                Arc::clone(&ledger),
                plugin_name.clone(),
            ));
        let assignment = Assignment {
            custody_type,
            payload,
            correlation_id,
            deadline,
            custody_state_reporter: reporter,
        };

        let handle: CustodyHandle = warden
            .take_custody(assignment)
            .await
            .map_err(StewardError::from)?;

        // Record the successful take in the ledger. UPSERT - if an
        // early state report from within the plugin's take_custody
        // already created a partial record, this call merges in the
        // metadata and preserves the earlier started_at and
        // last_state.
        ledger.record_custody(
            &plugin_name,
            &shelf_qualified,
            &handle,
            &custody_type_for_ledger,
        );

        // Emit the happening AFTER the ledger write so subscribers
        // that react by re-querying the ledger see the new record.
        // The owned Strings are moved into the happening since they
        // are not used again; only handle.id is cloned because
        // handle is returned to the caller.
        bus.emit(Happening::CustodyTaken {
            plugin: plugin_name,
            handle_id: handle.id.clone(),
            shelf: shelf_qualified,
            custody_type: custody_type_for_ledger,
            at: SystemTime::now(),
        });

        Ok(handle)
    }

    /// Deliver a course correction to an ongoing custody on the given
    /// shelf.
    ///
    /// Allocates a fresh correlation ID and constructs a
    /// [`CourseCorrection`] the warden receives. The caller is
    /// responsible for holding the [`CustodyHandle`] returned from
    /// the prior [`Self::take_custody`] and passing it back verbatim.
    ///
    /// Returns [`StewardError::Dispatch`] for the same shelf-kind
    /// mismatches as [`Self::take_custody`]. Returns
    /// [`StewardError::Plugin`] on warden-side rejection (unknown
    /// handle, correction-type not accepted, timeout, etc.).
    pub async fn course_correct(
        &mut self,
        shelf: &str,
        handle: &CustodyHandle,
        correction_type: String,
        payload: Vec<u8>,
    ) -> Result<(), StewardError> {
        let plugin = self.by_shelf.get_mut(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;
        let warden = match &mut plugin.handle {
            AdmittedHandle::Warden(w) => w,
            AdmittedHandle::Respondent(_) => {
                return Err(StewardError::Dispatch(format!(
                    "course_correct on shelf {shelf}: plugin is a \
                     respondent, not a warden"
                )));
            }
        };

        let correlation_id =
            self.custody_cid_counter.fetch_add(1, Ordering::Relaxed);
        let correction = CourseCorrection {
            correction_type,
            payload,
            correlation_id,
        };

        warden
            .course_correct(handle, correction)
            .await
            .map_err(Into::into)
    }

    /// Gracefully terminate an ongoing custody on the given shelf.
    ///
    /// The warden is expected to wind down the work identified by
    /// `handle`, emit a final state report, and return. After this
    /// future resolves successfully the handle is consumed; passing
    /// it to [`Self::course_correct`] or [`Self::release_custody`]
    /// again is a caller bug and the warden's response is
    /// implementation-defined.
    ///
    /// Returns [`StewardError::Dispatch`] for the same shelf-kind
    /// mismatches as [`Self::take_custody`]. Returns
    /// [`StewardError::Plugin`] on warden-side rejection.
    pub async fn release_custody(
        &mut self,
        shelf: &str,
        handle: CustodyHandle,
    ) -> Result<(), StewardError> {
        // Clone the ledger and bus Arcs up front for the same
        // reason as take_custody.
        let ledger = Arc::clone(&self.custody_ledger);
        let bus = Arc::clone(&self.happening_bus);

        let plugin = self.by_shelf.get_mut(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;
        let warden = match &mut plugin.handle {
            AdmittedHandle::Warden(w) => w,
            AdmittedHandle::Respondent(_) => {
                return Err(StewardError::Dispatch(format!(
                    "release_custody on shelf {shelf}: plugin is a \
                     respondent, not a warden"
                )));
            }
        };

        // Clone before handle is consumed by the warden call.
        let plugin_name = plugin.name.clone();
        let handle_id = handle.id.clone();

        warden
            .release_custody(handle)
            .await
            .map_err(StewardError::from)?;

        // Drop the record from the ledger after the warden
        // acknowledges the release. The removed record is
        // discarded; a future pass can expose it for archiving.
        ledger.release_custody(&plugin_name, &handle_id);

        // Emit the released happening after the ledger drop so
        // subscribers that react by re-querying the ledger see the
        // record gone. Both owned strings are moved in; neither is
        // used again.
        bus.emit(Happening::CustodyReleased {
            plugin: plugin_name,
            handle_id,
            at: SystemTime::now(),
        });

        Ok(())
    }

    /// Run a health check against every admitted plugin, returning a
    /// vector of (plugin name, report) pairs.
    pub async fn health_check_all(&self) -> Vec<(String, HealthReport)> {
        let mut out = Vec::with_capacity(self.by_shelf.len());
        for shelf in &self.admission_order {
            if let Some(p) = self.by_shelf.get(shelf) {
                let r = p.handle.health_check().await;
                out.push((p.name.clone(), r));
            }
        }
        out
    }

    /// Gracefully unload every admitted plugin in reverse admission
    /// order. Errors are logged but do not halt the shutdown sequence;
    /// the first error encountered (if any) is returned after all
    /// plugins have been attempted.
    ///
    /// For plugins spawned by the steward (via
    /// [`Self::admit_out_of_process_from_directory`]), the shutdown
    /// sequence is:
    ///
    /// 1. Send `unload` over the wire and await the response.
    /// 2. Drop the wire client, closing the connection. The child
    ///    sees EOF on its read half and its `serve()` returns.
    /// 3. Wait for the child to exit, with a bounded timeout.
    /// 4. If the timeout elapses, kill the child.
    ///
    /// Step 2 is essential: the child will not exit until its read
    /// side sees EOF, so waiting for the child while still holding
    /// the wire connection would deadlock.
    pub async fn shutdown(&mut self) -> Result<(), StewardError> {
        let mut first_err: Option<StewardError> = None;

        while let Some(shelf) = self.admission_order.pop() {
            if let Some(plugin) = self.by_shelf.remove(&shelf) {
                if let Err(e) = unload_one_plugin(plugin).await {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Timeout for child process exit during shutdown. After this elapses
/// the steward stops waiting politely and kills the child.
const CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for a freshly spawned plugin child to bind and accept on
/// its Unix socket.
const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval when waiting for a plugin socket to be ready.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Unload a single admitted plugin, handling both in-process and
/// out-of-process cases.
///
/// See [`AdmissionEngine::shutdown`] for the sequence rationale.
async fn unload_one_plugin(
    plugin: AdmittedPlugin,
) -> Result<(), StewardError> {
    let AdmittedPlugin {
        name,
        shelf,
        mut handle,
        child,
    } = plugin;

    tracing::info!(
        plugin = %name,
        shelf = %shelf,
        kind = %handle.kind_name(),
        "plugin unloading"
    );

    let unload_result = handle.unload().await;

    match &unload_result {
        Ok(()) => tracing::info!(plugin = %name, "plugin unloaded"),
        Err(e) => tracing::error!(
            plugin = %name,
            error = %e,
            "plugin unload failed"
        ),
    }

    // Drop the erased handle before waiting on the child. For
    // in-process plugins this is a no-op. For wire-backed plugins
    // this drops the WireClient, which closes the writer channel,
    // which drops the socket's writer half, which causes the child
    // to see EOF on its read side. Waiting on the child while still
    // holding the writer would deadlock.
    drop(handle);

    if let Some(mut child) = child {
        wait_or_kill_child(&name, &mut child).await;
    }

    unload_result.map_err(StewardError::Plugin)
}

/// Wait for a plugin's child process to exit; after a bounded timeout
/// kill it.
///
/// All errors are logged; none are propagated. The child is either
/// reaped cleanly or forcibly killed, in both cases leaving no zombie.
async fn wait_or_kill_child(name: &str, child: &mut Child) {
    match tokio::time::timeout(CHILD_SHUTDOWN_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            if status.success() {
                tracing::info!(
                    plugin = %name,
                    "plugin child exited cleanly"
                );
            } else {
                tracing::warn!(
                    plugin = %name,
                    ?status,
                    "plugin child exited with non-zero status"
                );
            }
        }
        Ok(Err(e)) => {
            tracing::error!(
                plugin = %name,
                error = %e,
                "plugin child wait failed"
            );
        }
        Err(_) => {
            tracing::warn!(
                plugin = %name,
                timeout_ms = CHILD_SHUTDOWN_TIMEOUT.as_millis() as u64,
                "plugin child did not exit after disconnection, killing"
            );
            if let Err(e) = child.kill().await {
                tracing::error!(
                    plugin = %name,
                    error = %e,
                    "plugin child kill failed"
                );
            }
            // Reap the zombie regardless of kill success.
            let _ = child.wait().await;
        }
    }
}

/// Poll the plugin socket until the child binds it, or the child
/// exits, or the timeout elapses.
///
/// Returns the connected stream on success. On failure, the caller
/// is responsible for killing and reaping the child.
async fn wait_for_socket_ready(
    socket_path: &Path,
    child: &mut Child,
) -> Result<UnixStream, StewardError> {
    let deadline = Instant::now() + SOCKET_READY_TIMEOUT;
    loop {
        // If the child has already exited, stop polling.
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(StewardError::Admission(format!(
                    "plugin child exited before socket was ready: {status:?}"
                )));
            }
            Ok(None) => {} // still running
            Err(e) => {
                return Err(StewardError::Admission(format!(
                    "error polling plugin child: {e}"
                )));
            }
        }

        match UnixStream::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() >= deadline => {
                return Err(StewardError::Admission(format!(
                    "timed out waiting for plugin socket at {} after {:?}",
                    socket_path.display(),
                    SOCKET_READY_TIMEOUT
                )));
            }
            Err(_) => {
                tokio::time::sleep(SOCKET_POLL_INTERVAL).await;
            }
        }
    }
}

/// Build a v0 LoadContext for a plugin.
///
/// The context carries per-plugin filesystem paths, empty config, no
/// deadline, logging-only implementations of the state and interaction
/// callbacks, and real announcers backed by the supplied registry and
/// graph.
fn build_load_context(
    manifest: &Manifest,
    registry: Arc<SubjectRegistry>,
    graph: Arc<RelationGraph>,
) -> LoadContext {
    let state_dir = PathBuf::from("/var/lib/evo/plugins")
        .join(&manifest.plugin.name)
        .join("state");
    let credentials_dir = PathBuf::from("/var/lib/evo/plugins")
        .join(&manifest.plugin.name)
        .join("credentials");

    LoadContext {
        config: toml::Table::new(),
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
        subject_announcer: Arc::new(RegistrySubjectAnnouncer::new(
            Arc::clone(&registry),
            manifest.plugin.name.clone(),
        )),
        relation_announcer: Arc::new(RegistryRelationAnnouncer::new(
            registry,
            graph,
            manifest.plugin.name.clone(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{
        BuildInfo, PluginIdentity, RuntimeCapabilities,
    };

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
            async move {
                Ok(Response::for_request(req, req.payload.clone()))
            }
        }
    }

    fn test_catalogue() -> Catalogue {
        Catalogue::from_toml(
            r#"
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
"#,
        )
        .unwrap()
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
        let mut engine = AdmissionEngine::new();
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let manifest = test_manifest("org.test.ping");
        engine
            .admit_singleton_respondent(plugin, manifest, &catalogue)
            .await
            .unwrap();
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn rejects_plugin_with_identity_mismatch() {
        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::new();
        let plugin = TestRespondent {
            // describe() will return this name
            name: "org.test.actual".into(),
            ..Default::default()
        };
        // But manifest says a different name
        let manifest = test_manifest("org.test.claimed");
        let r = engine
            .admit_singleton_respondent(plugin, manifest, &catalogue)
            .await;
        assert!(matches!(r, Err(StewardError::Admission(_))));
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn rejects_plugin_targeting_missing_shelf() {
        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::new();
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
        let r = engine
            .admit_singleton_respondent(plugin, manifest, &catalogue)
            .await;
        assert!(matches!(r, Err(StewardError::Admission(_))));
    }

    #[tokio::test]
    async fn rejects_duplicate_shelf_admission() {
        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::new();

        let p1 = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(
                p1,
                test_manifest("org.test.ping"),
                &catalogue,
            )
            .await
            .unwrap();

        let p2 = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        let r = engine
            .admit_singleton_respondent(
                p2,
                test_manifest("org.test.ping"),
                &catalogue,
            )
            .await;
        assert!(matches!(r, Err(StewardError::Admission(_))));
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn handle_request_dispatches() {
        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::new();
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(
                plugin,
                test_manifest("org.test.ping"),
                &catalogue,
            )
            .await
            .unwrap();

        let req = Request {
            request_type: "ping".into(),
            payload: b"hello".to_vec(),
            correlation_id: 1,
            deadline: None,
        };
        let resp = engine.handle_request("test.ping", req).await.unwrap();
        assert_eq!(resp.payload, b"hello");
    }

    #[tokio::test]
    async fn handle_request_unknown_shelf_errors() {
        let mut engine = AdmissionEngine::new();
        let req = Request {
            request_type: "ping".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,
        };
        let r = engine.handle_request("missing.shelf", req).await;
        assert!(matches!(r, Err(StewardError::Dispatch(_))));
    }

    #[tokio::test]
    async fn shutdown_unloads_everything() {
        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::new();
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(
                plugin,
                test_manifest("org.test.ping"),
                &catalogue,
            )
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
        {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_
        {
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
        let mut engine = AdmissionEngine::new();

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
            .admit_singleton_respondent(
                plugin,
                test_manifest("org.test.ping"),
                &catalogue,
            )
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
    async fn with_registry_shares_registry_across_engines() {
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
        let mut engine = AdmissionEngine::with_registry(Arc::clone(&shared));
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(
                plugin,
                test_manifest("org.test.ping"),
                &catalogue,
            )
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
        {
            async move { Ok(()) }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_
        {
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
        let mut engine = AdmissionEngine::new();

        let plugin = RelatingRespondent {
            name: "org.test.ping".into(),
        };
        engine
            .admit_singleton_respondent(
                plugin,
                test_manifest("org.test.ping"),
                &catalogue,
            )
            .await
            .unwrap();

        let registry = engine.registry();
        let graph = engine.relation_graph();
        assert_eq!(registry.subject_count(), 2);
        assert_eq!(graph.relation_count(), 1);

        let source_id = registry
            .resolve(&evo_plugin_sdk::contract::ExternalAddressing::new(
                "s",
                "track-1",
            ))
            .unwrap();
        let target_id = registry
            .resolve(&evo_plugin_sdk::contract::ExternalAddressing::new(
                "s",
                "album-1",
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
    fn example_catalogue() -> Catalogue {
        Catalogue::from_toml(
            r#"
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
        .unwrap()
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
    fn example_warden_manifest_with_transport(
        transport_block: &str,
    ) -> String {
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

        let mut engine = AdmissionEngine::new();
        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
                &catalogue,
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
        std::fs::write(
            plugin_dir.path().join("manifest.toml"),
            &manifest_text,
        )
        .unwrap();

        let catalogue = example_catalogue();
        let mut engine = AdmissionEngine::new();
        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
                &catalogue,
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
        std::fs::write(
            plugin_dir.path().join("manifest.toml"),
            &manifest_text,
        )
        .unwrap();

        let catalogue = example_catalogue();
        let mut engine = AdmissionEngine::new();
        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
                &catalogue,
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
        // Pass 4e-c made admit_out_of_process_from_directory branch
        // on manifest.kind.interaction. This test verifies that
        // warden manifests get past the early validation (manifest
        // parse, transport.kind check) and reach the spawn step.
        // True end-to-end branch coverage (warden manifest routed to
        // admit_out_of_process_warden, describe handshake, load)
        // lives in the example-warden tests introduced in pass 4f,
        // where a real warden binary is available.
        let plugin_dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = tempfile::TempDir::new().unwrap();
        let manifest_text = example_warden_manifest_with_transport(
            "[transport]\ntype = \"out-of-process\"\nexec = \"nonexistent-warden-binary-xyz\"",
        );
        std::fs::write(
            plugin_dir.path().join("manifest.toml"),
            &manifest_text,
        )
        .unwrap();

        let catalogue = example_catalogue();
        let mut engine = AdmissionEngine::new();
        let r = engine
            .admit_out_of_process_from_directory(
                plugin_dir.path(),
                runtime_dir.path(),
                &catalogue,
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
    // Exercise the in-process warden path introduced in pass 4e:
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
            async move { Ok(()) }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
        {
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
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
            async move { Ok(()) }
        }

        fn release_custody<'a>(
            &'a mut self,
            _handle: CustodyHandle,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();
        assert_eq!(engine.len(), 1);
    }

    #[tokio::test]
    async fn admit_singleton_warden_rejects_respondent_manifest() {
        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::new();
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
                &catalogue,
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
        let mut engine = AdmissionEngine::new();
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
                &catalogue,
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

    #[tokio::test]
    async fn take_custody_dispatches_to_warden() {
        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();

        let handle = engine
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();

        let handle = engine
            .take_custody(
                "test.custody",
                "playback".into(),
                vec![],
                None,
            )
            .await
            .unwrap();
        engine
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();

        let handle = engine
            .take_custody(
                "test.custody",
                "playback".into(),
                vec![],
                None,
            )
            .await
            .unwrap();
        engine.release_custody("test.custody", handle).await.unwrap();
    }

    #[tokio::test]
    async fn handle_request_on_warden_shelf_errors() {
        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();

        let req = Request {
            request_type: "ping".into(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,
        };
        let r = engine.handle_request("test.custody", req).await;
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
        let mut engine = AdmissionEngine::new();
        let plugin = TestRespondent {
            name: "org.test.ping".into(),
            ..Default::default()
        };
        engine
            .admit_singleton_respondent(
                plugin,
                test_manifest("org.test.ping"),
                &catalogue,
            )
            .await
            .unwrap();

        let r = engine
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();
        assert_eq!(engine.len(), 1);
        engine.shutdown().await.unwrap();
        assert_eq!(engine.len(), 0);
    }

    // -----------------------------------------------------------------
    // Custody ledger integration tests (pass 4g-b).
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();
        assert_eq!(engine.custody_ledger().len(), 0);

        let handle = engine
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();

        let handle = engine
            .take_custody(
                "test.custody",
                "playback".into(),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(engine.custody_ledger().len(), 1);

        engine
            .release_custody("test.custody", handle)
            .await
            .unwrap();
        assert_eq!(engine.custody_ledger().len(), 0);
    }

    #[tokio::test]
    async fn with_registry_graph_and_ledger_shares_ledger() {
        use crate::custody::CustodyLedger;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ledger = Arc::new(CustodyLedger::new());

        let catalogue = test_catalogue();
        let mut engine = AdmissionEngine::with_registry_graph_and_ledger(
            Arc::clone(&registry),
            Arc::clone(&graph),
            Arc::clone(&ledger),
        );

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
                &catalogue,
            )
            .await
            .unwrap();

        let _handle = engine
            .take_custody(
                "test.custody",
                "playback".into(),
                vec![],
                None,
            )
            .await
            .unwrap();

        // Externally-held handle reflects the engine's recording.
        assert_eq!(ledger.len(), 1);
    }

    // -----------------------------------------------------------------
    // Happenings bus integration tests (pass 5b).
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();

        // Subscribe BEFORE take_custody so the happening reaches us.
        let mut rx = engine.happening_bus().subscribe();

        let handle = engine
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();

        let handle = engine
            .take_custody(
                "test.custody",
                "playback".into(),
                vec![],
                None,
            )
            .await
            .unwrap();

        // Subscribe AFTER the take so only the release reaches us.
        // This also verifies the "late subscriber misses earlier
        // happenings" property at the engine level.
        let mut rx = engine.happening_bus().subscribe();
        let handle_id = handle.id.clone();

        engine
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
        let mut engine = AdmissionEngine::new();
        let warden = TestWarden {
            name: "org.test.custody".into(),
        };
        engine
            .admit_singleton_warden(
                warden,
                test_warden_manifest("org.test.custody"),
                &catalogue,
            )
            .await
            .unwrap();

        let mut rx = engine.happening_bus().subscribe();

        let handle = engine
            .take_custody(
                "test.custody",
                "playback".into(),
                vec![],
                None,
            )
            .await
            .unwrap();
        engine
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
    async fn with_registry_graph_ledger_and_bus_shares_bus() {
        use crate::custody::CustodyLedger;
        use crate::happenings::HappeningBus;

        let registry = Arc::new(SubjectRegistry::new());
        let graph = Arc::new(RelationGraph::new());
        let ledger = Arc::new(CustodyLedger::new());
        let bus = Arc::new(HappeningBus::new());

        let catalogue = test_catalogue();
        let mut engine =
            AdmissionEngine::with_registry_graph_ledger_and_bus(
                Arc::clone(&registry),
                Arc::clone(&graph),
                Arc::clone(&ledger),
                Arc::clone(&bus),
            );

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
                &catalogue,
            )
            .await
            .unwrap();
        let _handle = engine
            .take_custody(
                "test.custody",
                "playback".into(),
                vec![],
                None,
            )
            .await
            .unwrap();

        let got = rx.recv().await.expect("recv from externally-held bus");
        assert!(matches!(
            got,
            crate::happenings::Happening::CustodyTaken { .. }
        ));
    }
}
