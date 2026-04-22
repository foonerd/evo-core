//! The admission engine.
//!
//! The admission engine owns the admitted plugins and runs their
//! lifecycles. For v0 it supports only in-process singleton respondents;
//! wardens, factories, out-of-process plugins, and multi-plugin shelves
//! are future work.
//!
//! ## Type erasure
//!
//! The SDK's public traits (`Plugin`, `Respondent`) use native async in
//! traits with `impl Future + Send` return types. Those traits are not
//! object-safe (native async traits cannot be `dyn`) so the admission
//! engine cannot hold `Box<dyn Respondent>` directly.
//!
//! The engine solves this with an internal object-safe trait
//! [`ErasedRespondent`] and a generic adapter [`RespondentAdapter`] that
//! implements it for any concrete `T: Respondent + 'static`. This keeps
//! the public SDK traits zero-allocation while letting the engine store
//! heterogeneous plugins in a single collection.

use crate::catalogue::Catalogue;
use crate::context::{
    LoggingInstanceAnnouncer, LoggingStateReporter,
    LoggingUserInteractionRequester, RegistryRelationAnnouncer,
    RegistrySubjectAnnouncer,
};
use crate::error::StewardError;
use crate::relations::RelationGraph;
use crate::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::{
    HealthReport, LoadContext, Plugin, PluginDescription, PluginError,
    Request, Respondent, Response,
};
use evo_plugin_sdk::manifest::TransportKind;
use evo_plugin_sdk::Manifest;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// Internal record of one admitted plugin.
struct AdmittedPlugin {
    /// Canonical plugin name, per the manifest.
    name: String,
    /// Fully-qualified shelf this plugin occupies (`<rack>.<shelf>`).
    shelf: String,
    /// Type-erased handle for lifecycle and request dispatch.
    erased: Box<dyn ErasedRespondent>,
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
            .field("erased", &"<Box<dyn ErasedRespondent>>")
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
}

impl Default for AdmissionEngine {
    fn default() -> Self {
        Self {
            by_shelf: HashMap::new(),
            admission_order: Vec::new(),
            registry: Arc::new(SubjectRegistry::new()),
            relation_graph: Arc::new(RelationGraph::new()),
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

        let mut erased: Box<dyn ErasedRespondent> =
            Box::new(RespondentAdapter::new(plugin));

        let description = erased.describe().await;
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
        erased.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

        let record = AdmittedPlugin {
            name: manifest.plugin.name.clone(),
            shelf: shelf_qualified.clone(),
            erased,
            child: None,
        };

        tracing::info!(
            plugin = %record.name,
            shelf = %record.shelf,
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

        let mut erased: Box<dyn ErasedRespondent> = Box::new(respondent);

        let ctx = build_load_context(
            &manifest,
            Arc::clone(&self.registry),
            Arc::clone(&self.relation_graph),
        );
        erased.load(&ctx).await.map_err(|e| {
            StewardError::Admission(format!(
                "{}: load failed: {}",
                manifest.plugin.name, e
            ))
        })?;

        let record = AdmittedPlugin {
            name: manifest.plugin.name.clone(),
            shelf: shelf_qualified.clone(),
            erased,
            child: None,
        };

        tracing::info!(
            plugin = %record.name,
            shelf = %record.shelf,
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
    ///    `into_split()` and hands the owned halves to
    ///    [`Self::admit_out_of_process_respondent`].
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

        // Hand off to the existing wire-admission path. On failure,
        // kill+reap the child.
        if let Err(e) = self
            .admit_out_of_process_respondent(
                manifest.clone(),
                catalogue,
                reader,
                writer,
            )
            .await
        {
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
    /// Returns an error if no plugin is admitted on that shelf, or if the
    /// plugin returns an error.
    pub async fn handle_request(
        &mut self,
        shelf: &str,
        request: Request,
    ) -> Result<Response, StewardError> {
        let plugin = self.by_shelf.get_mut(shelf).ok_or_else(|| {
            StewardError::Dispatch(format!("no plugin on shelf: {shelf}"))
        })?;
        plugin.erased.handle_request(&request).await.map_err(Into::into)
    }

    /// Run a health check against every admitted plugin, returning a
    /// vector of (plugin name, report) pairs.
    pub async fn health_check_all(&self) -> Vec<(String, HealthReport)> {
        let mut out = Vec::with_capacity(self.by_shelf.len());
        for shelf in &self.admission_order {
            if let Some(p) = self.by_shelf.get(shelf) {
                let r = p.erased.health_check().await;
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
        mut erased,
        child,
    } = plugin;

    tracing::info!(
        plugin = %name,
        shelf = %shelf,
        "plugin unloading"
    );

    let unload_result = erased.unload().await;

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
    drop(erased);

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
}
