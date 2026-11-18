// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! A plugin's nested dispatch to a household network writer, over
//! real HTTPS.
//!
//! A plugin that reaches another plugin's verb through its load
//! context's shelf request dispatcher does so as the framework's
//! `plugin-system` principal, whoever asked the plugin. That principal
//! holds `network_admin` at step-up rank, so a nest can reach a
//! `write:network_admin` writer.
//!
//! This file proves it end to end, under both choices a distribution
//! has for its LAN:
//!
//! - the caller is anonymous, from loopback, and the HTTPS middleware
//!   mints its principal;
//! - it asks a relay verb that declares no capability;
//! - the relay plugin nests a dispatch to a `write:network_admin` verb
//!   on another plugin's shelf;
//! - both plugins are admitted through `admit_singleton_respondent`
//!   from real manifests, and the dispatcher is the steward's own,
//!   wired as `run()` wires it.
//!
//! Under `LanPrivilege::Withheld` the caller itself is refused the
//! writer. The nest still reaches it, which is what shows the grant is
//! the plugin-system principal's and not the caller's.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::household_protection::HouseholdProtectionRuntime;
use evo::persistence::MemoryPersistenceStore;
use evo::projections::ProjectionEngine;
use evo::relations::RelationGraph;
use evo::server::Server;
use evo::shelf_dispatcher::StewardShelfRequestDispatcher;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo::{maybe_boot_https, LanPrivilege};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities, ShelfRequestDispatcher,
};
use evo_plugin_sdk::Manifest;
use serde_json::{json, Value};
use tokio::sync::Mutex;

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const WRITER: &str = "org.test.nest.network";
const WRITER_SHELF: &str = "nest.network";
/// `write:network_admin`: the household network door.
const WRITE_VERB: &str = "set_network";

const RELAY: &str = "org.test.nest.relay";
const RELAY_SHELF: &str = "nest.relay";
/// No capability: any admitted caller may ask it.
const RELAY_VERB: &str = "relay";

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "nest"
family = "domain"
charter = "Two shelves: a network writer, and a relay that nests a dispatch to it."

[[racks.shelves]]
name = "network"
shape = 1
description = "Carries one write:network_admin verb."

[[racks.shelves]]
name = "relay"
shape = 1
description = "Carries one verb with no capability, which nests a dispatch."
"#;

fn manifest(name: &str, shelf: &str, verb: &str, capability: &str) -> Manifest {
    let toml = format!(
        r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "{shelf}"
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
autostart = true
restart_on_crash = false
restart_budget = 0

[capabilities.respondent]
request_types = ["{verb}"]
response_budget_ms = 30000
{capability}
"#
    );
    Manifest::from_toml(&toml).expect("nest manifest must parse")
}

fn description(name: &str, verb: &str) -> PluginDescription {
    PluginDescription {
        identity: PluginIdentity {
            name: name.to_string(),
            version: semver::Version::new(0, 1, 0),
            contract: 1,
        },
        runtime_capabilities: RuntimeCapabilities {
            request_types: vec![verb.to_string()],
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

/// The network writer. Counts what reaches it, so "admitted" is read
/// from evidence rather than from a response shape.
struct Writer {
    reached: Arc<AtomicUsize>,
}

impl Plugin for Writer {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move { description(WRITER, WRITE_VERB) }
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

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move { HealthReport::healthy() }
    }
}

impl Respondent for Writer {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            self.reached.fetch_add(1, Ordering::SeqCst);
            Ok(Response::for_request(
                req,
                json!({"set": true}).to_string().into_bytes(),
            ))
        }
    }
}

/// The relay. Asked for its one verb, it nests a dispatch to the
/// writer through the dispatcher its load context carries, and answers
/// with what the nest came back with.
struct Relay {
    dispatcher: Option<Arc<dyn ShelfRequestDispatcher>>,
}

impl Plugin for Relay {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move { description(RELAY, RELAY_VERB) }
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            self.dispatcher = ctx.shelf_request_dispatcher.clone();
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move { Ok(()) }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move { HealthReport::healthy() }
    }
}

impl Respondent for Relay {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let dispatcher = self.dispatcher.as_ref().ok_or_else(|| {
                PluginError::Permanent(
                    "no shelf request dispatcher on the load context".into(),
                )
            })?;
            let nested = match dispatcher
                .dispatch(WRITER_SHELF, WRITE_VERB, b"{}".to_vec(), None)
                .await
            {
                Ok(bytes) => json!({
                    "nest": "admitted",
                    "answer": serde_json::from_slice::<Value>(&bytes)
                        .unwrap_or(Value::Null),
                }),
                Err(e) => json!({"nest": "refused", "error": e.to_string()}),
            };
            Ok(Response::for_request(req, nested.to_string().into_bytes()))
        }
    }
}

struct Booted {
    addr: std::net::SocketAddr,
    ca_pem: String,
    reached: Arc<AtomicUsize>,
    // Held so the Weak the dispatcher keeps stays upgradable.
    _server: Arc<Server>,
}

/// Boot a steward with the writer and the relay admitted through the
/// public admission path, the steward's own shelf dispatcher threaded
/// into both load contexts, a household runtime at its first-start
/// level, and the HTTPS listener mounted with `lan_privilege`.
async fn boot(lan_privilege: LanPrivilege) -> (Booted, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("state tempdir");
    let parent = dir.path().to_path_buf();
    std::env::set_var("EVO_HTTPS_LISTEN_ADDR", "127.0.0.1:0");
    std::env::set_var(
        "EVO_HTTPS_STATE_DIR",
        parent.to_string_lossy().to_string(),
    );

    let catalogue =
        Arc::new(Catalogue::from_toml(CATALOGUE_TOML).expect("catalogue"));
    let state = StewardState::builder()
        .catalogue(catalogue)
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .persistence(Arc::new(MemoryPersistenceStore::new()))
        .claimant_issuer(Arc::new(evo::claimant::ClaimantTokenIssuer::new(
            "plugin-system-nest",
        )))
        .build()
        .expect("steward state");

    let dispatcher = StewardShelfRequestDispatcher::new();
    let mut engine = AdmissionEngine::new(
        Arc::clone(&state),
        parent.join("data-root"),
        std::path::PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    )
    .with_shelf_request_dispatcher(
        Arc::clone(&dispatcher) as Arc<dyn ShelfRequestDispatcher>
    );
    let reached = Arc::new(AtomicUsize::new(0));
    engine
        .admit_singleton_respondent(
            Writer {
                reached: Arc::clone(&reached),
            },
            manifest(
                WRITER,
                WRITER_SHELF,
                WRITE_VERB,
                &format!(
                    "\n[capabilities.respondent.verb_capabilities.{WRITE_VERB}]\n\
                     kind = \"write\"\nscope = \"network_admin\"\n"
                ),
            ),
        )
        .await
        .expect("admit the network writer");
    engine
        .admit_singleton_respondent(
            Relay { dispatcher: None },
            manifest(RELAY, RELAY_SHELF, RELAY_VERB, ""),
        )
        .await
        .expect("admit the relay");

    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let router = Arc::clone(engine.router());

    // The first-start household runtime: attached, as on every
    // steward, at the level that protects nothing.
    let runtime = Arc::new(HouseholdProtectionRuntime::load(
        parent.clone(),
        None,
        evo::https_boot::operator_bootstrap_capability_set(),
    ));
    let auth = Arc::new(evo::auth::AuthSessionStore::with_defaults());
    let server = Arc::new(
        Server::new(
            parent.join("unused.sock"),
            router,
            Arc::clone(&state),
            projections,
        )
        .with_household_protection(runtime)
        .with_auth(None, auth),
    );
    // As `run()` does once the Server exists.
    dispatcher.set_server(&server);

    let handles = maybe_boot_https(
        Arc::clone(&server),
        &parent.join("persistence.sqlite"),
        None,
        None,
        lan_privilege,
    )
    .await
    .expect("maybe_boot_https Ok")
    .expect("HTTPS listener mounted");
    let _ = state.bearer_token_issuer.set(Arc::clone(&handles.issuer));

    let addr = handles.server.local_addr;
    let ca_pem =
        std::fs::read_to_string(parent.join("https").join("ca.crt")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (
        Booted {
            addr,
            ca_pem,
            reached,
            _server: server,
        },
        dir,
    )
}

fn client_with_ca(ca_pem: &str) -> reqwest::Client {
    let cert = reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap();
    reqwest::Client::builder()
        .add_root_certificate(cert)
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .pool_max_idle_per_host(0)
        .build()
        .unwrap()
}

/// Ask `verb` on `shelf` with no bearer, from loopback.
async fn ask_anonymously(
    booted: &Booted,
    shelf: &str,
    verb: &str,
) -> (u16, Value) {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let resp = client_with_ca(&booted.ca_pem)
        .post(format!("https://{}/api/v1/request", booted.addr))
        .json(&json!({
            "shelf": shelf,
            "request_type": verb,
            "payload_b64": B64.encode(b"{}"),
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let payload = body
        .get("payload_b64")
        .and_then(Value::as_str)
        .map(|b| serde_json::from_slice(&B64.decode(b).unwrap()).unwrap())
        .unwrap_or(body);
    (status, payload)
}

/// The relay's nest reaches the writer, once, and says so.
async fn the_nest_reaches_the_writer(booted: &Booted) {
    let before = booted.reached.load(Ordering::SeqCst);
    let (status, answer) =
        ask_anonymously(booted, RELAY_SHELF, RELAY_VERB).await;
    assert_eq!(status, 200, "answer={answer}");
    assert_eq!(
        answer["nest"], "admitted",
        "plugin-system must carry network_admin into the nest. answer={answer}"
    );
    assert_eq!(answer["answer"], json!({"set": true}), "answer={answer}");
    assert_eq!(
        booted.reached.load(Ordering::SeqCst),
        before + 1,
        "the writer was reached by the nest"
    );
}

#[tokio::test]
async fn a_nested_network_write_is_admitted_with_lan_privilege_policed() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(LanPrivilege::PolicedByHousehold).await;
    the_nest_reaches_the_writer(&booted).await;
}

#[tokio::test]
async fn a_nested_network_write_is_admitted_with_lan_privilege_withheld() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(LanPrivilege::Withheld).await;

    // The caller itself is refused the writer: withheld, it holds no
    // network_admin.
    let (status, refused) =
        ask_anonymously(&booted, WRITER_SHELF, WRITE_VERB).await;
    assert_eq!(status, 403, "body={refused}");
    assert_eq!(
        refused["error"]["details"]["subclass"],
        "verb_capability_scope_not_granted",
        "body={refused}"
    );
    assert_eq!(booted.reached.load(Ordering::SeqCst), 0);

    // The same caller, through the relay, reaches it: the nest's grant is
    // plugin-system's, not the caller's.
    the_nest_reaches_the_writer(&booted).await;
}
