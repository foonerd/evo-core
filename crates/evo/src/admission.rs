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
use evo_plugin_sdk::Manifest;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

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
}

impl std::fmt::Debug for AdmittedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedPlugin")
            .field("name", &self.name)
            .field("shelf", &self.shelf)
            .field("erased", &"<Box<dyn ErasedRespondent>>")
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
    pub async fn shutdown(&mut self) -> Result<(), StewardError> {
        let mut first_err: Option<StewardError> = None;

        while let Some(shelf) = self.admission_order.pop() {
            if let Some(mut plugin) = self.by_shelf.remove(&shelf) {
                tracing::info!(
                    plugin = %plugin.name,
                    shelf = %plugin.shelf,
                    "plugin unloading"
                );
                match plugin.erased.unload().await {
                    Ok(()) => {
                        tracing::info!(
                            plugin = %plugin.name,
                            "plugin unloaded"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            plugin = %plugin.name,
                            error = %e,
                            "plugin unload failed"
                        );
                        if first_err.is_none() {
                            first_err = Some(StewardError::Plugin(e));
                        }
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
}
