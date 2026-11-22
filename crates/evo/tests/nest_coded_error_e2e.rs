// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! A plugin's error keeps its code and the values it names, on the
//! operator wire and through a nested dispatch, for every class.
//!
//! Two in-process plugins on a steward booted over real HTTPS: a source
//! whose one verb fails as asked, under the code `share_busy` with the
//! values `share` and `minutes`, and a relay that nests a dispatch to
//! the source and answers with what came back. The relay asks either
//! `dispatch_coded`, which keeps the code and values, or `dispatch`,
//! which answers as it always has.

use std::collections::BTreeMap;
use std::future::Future;
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
    RuntimeCapabilities, ShelfDispatchError, ShelfRequestDispatcher,
};
use evo_plugin_sdk::error_taxonomy::ErrorClass;
use evo_plugin_sdk::Manifest;
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// The boot reads its listener from the environment, which is
/// process-wide.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const SOURCE: &str = "org.test.coded.source";
const SOURCE_SHELF: &str = "coded.source";
const FAIL_VERB: &str = "fail";
const RELAY: &str = "org.test.coded.relay";
const RELAY_SHELF: &str = "coded.relay";
const RELAY_VERB: &str = "relay";

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "coded"
family = "domain"
charter = "Two shelves for the coded error pins."

[[racks.shelves]]
name = "source"
shape = 1
description = "Carries one verb that fails as asked."

[[racks.shelves]]
name = "relay"
shape = 1
description = "Carries one verb that nests a dispatch to the source."
"#;

fn manifest(name: &str, shelf: &str, verb: &str) -> Manifest {
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
"#
    );
    Manifest::from_toml(&toml).expect("manifest must parse")
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

/// The values the source's error names.
fn named() -> BTreeMap<String, Value> {
    [
        ("share".to_string(), json!("Music")),
        ("minutes".to_string(), json!(30)),
    ]
    .into_iter()
    .collect()
}

/// Fails as its payload asks: `{"class": "<wire class>", "values":
/// <bool>}`. With values it fails `WithValues`, without them
/// `WithSubclass`, under the code `share_busy` either way.
struct Source;

impl Plugin for Source {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move { description(SOURCE, FAIL_VERB) }
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

impl Respondent for Source {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let asked: Value =
                serde_json::from_slice(&req.payload).unwrap_or(Value::Null);
            let class: ErrorClass = serde_json::from_value(
                asked["class"].clone(),
            )
            .map_err(|e| PluginError::Permanent(format!("class: {e}")))?;
            let subclass = "share_busy".to_string();
            let message = "the share is in use".to_string();
            Err(if asked["values"] == json!(true) {
                PluginError::WithValues {
                    class,
                    subclass,
                    message,
                    values: named(),
                }
            } else {
                PluginError::WithSubclass {
                    class,
                    subclass,
                    message,
                }
            })
        }
    }
}

/// Nests a dispatch to the source with `ask` as its payload, by
/// `dispatch_coded` when `coded` is true and by `dispatch` otherwise,
/// and answers with what came back.
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

/// The variant name of a dispatch failure.
fn kind_of(e: &ShelfDispatchError) -> Value {
    match e {
        ShelfDispatchError::Transient { .. } => json!("transient"),
        ShelfDispatchError::Permanent { .. } => json!("permanent"),
        ShelfDispatchError::Refused { subclass, .. } => {
            json!({ "refused": subclass })
        }
        ShelfDispatchError::SubstrateFailure { .. } => json!("substrate"),
        other => json!(format!("{other:?}")),
    }
}

impl Respondent for Relay {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let dispatcher = self.dispatcher.as_ref().ok_or_else(|| {
                PluginError::Permanent("no shelf request dispatcher".into())
            })?;
            let asked: Value =
                serde_json::from_slice(&req.payload).unwrap_or(Value::Null);
            let ask = asked["ask"].to_string().into_bytes();
            let answer = if asked["coded"] == json!(true) {
                match dispatcher
                    .dispatch_coded(SOURCE_SHELF, FAIL_VERB, ask, None)
                    .await
                {
                    Ok(_) => json!({ "nest": "answered" }),
                    Err(e) => json!({
                        "kind": kind_of(&e.kind),
                        "class": e.class,
                        "subclass": e.subclass,
                        "values": e.values,
                    }),
                }
            } else {
                match dispatcher
                    .dispatch(SOURCE_SHELF, FAIL_VERB, ask, None)
                    .await
                {
                    Ok(_) => json!({ "nest": "answered" }),
                    Err(e) => json!({ "kind": kind_of(&e) }),
                }
            };
            Ok(Response::for_request(req, answer.to_string().into_bytes()))
        }
    }
}

struct Booted {
    addr: std::net::SocketAddr,
    ca_pem: String,
    _server: Arc<Server>,
    _handles: evo::https_boot::HttpsBootHandles,
}

async fn boot() -> (Booted, tempfile::TempDir) {
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
            "nest-coded-error",
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
    engine
        .admit_singleton_respondent(
            Source,
            manifest(SOURCE, SOURCE_SHELF, FAIL_VERB),
        )
        .await
        .expect("admit the source");
    engine
        .admit_singleton_respondent(
            Relay { dispatcher: None },
            manifest(RELAY, RELAY_SHELF, RELAY_VERB),
        )
        .await
        .expect("admit the relay");

    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let runtime = Arc::new(HouseholdProtectionRuntime::load(
        parent.clone(),
        None,
        evo::https_boot::operator_bootstrap_capability_set(),
    ));
    let server = Arc::new(
        Server::new(
            parent.join("unused.sock"),
            Arc::clone(engine.router()),
            Arc::clone(&state),
            projections,
        )
        .with_household_protection(runtime)
        .with_auth(
            None,
            Arc::new(evo::auth::AuthSessionStore::with_defaults()),
        ),
    );
    dispatcher.set_server(&server);

    let handles = maybe_boot_https(
        Arc::clone(&server),
        &parent.join("persistence.sqlite"),
        None,
        None,
        LanPrivilege::default(),
        None,
    )
    .await
    .expect("maybe_boot_https Ok")
    .expect("HTTPS listener mounted");
    let addr = handles.server.local_addr;
    let ca_pem =
        std::fs::read_to_string(parent.join("https").join("ca.crt")).unwrap();
    (
        Booted {
            addr,
            ca_pem,
            _server: server,
            _handles: handles,
        },
        dir,
    )
}

/// Ask `verb` on `shelf` with `payload`, from loopback with no bearer.
/// Returns the status and the decoded answer, or the error envelope.
async fn ask(
    booted: &Booted,
    shelf: &str,
    verb: &str,
    payload: &Value,
) -> (u16, Value) {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let client = reqwest::Client::builder()
        .add_root_certificate(
            reqwest::Certificate::from_pem(booted.ca_pem.as_bytes()).unwrap(),
        )
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .build()
        .unwrap();
    let resp = client
        .post(format!("https://{}/api/v1/request", booted.addr))
        .json(&json!({
            "shelf": shelf,
            "request_type": verb,
            "payload_b64": B64.encode(payload.to_string()),
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let answer = body
        .get("payload_b64")
        .and_then(Value::as_str)
        .map(|b| serde_json::from_slice(&B64.decode(b).unwrap()).unwrap())
        .unwrap_or(body);
    (status, answer)
}

#[tokio::test]
async fn an_error_reaches_the_operator_wire_with_its_values() {
    let _env = ENV_LOCK.lock().await;
    let (booted, _dir) = boot().await;

    let (_, body) = ask(
        &booted,
        SOURCE_SHELF,
        FAIL_VERB,
        &json!({ "class": "transient", "values": true }),
    )
    .await;
    assert_eq!(body["error"]["class"], "transient", "{body}");
    assert_eq!(body["error"]["message"], "the share is in use", "{body}");
    assert_eq!(body["error"]["details"]["subclass"], "share_busy", "{body}");
    assert_eq!(
        body["error"]["details"]["values"],
        json!({ "share": "Music", "minutes": 30 }),
        "{body}"
    );

    // WithSubclass: the subclass alone, as it always was.
    let (_, body) = ask(
        &booted,
        SOURCE_SHELF,
        FAIL_VERB,
        &json!({ "class": "transient", "values": false }),
    )
    .await;
    assert_eq!(
        body["error"]["details"],
        json!({ "subclass": "share_busy" })
    );
}

#[tokio::test]
async fn a_nest_keeps_the_code_and_values_for_every_class() {
    let _env = ENV_LOCK.lock().await;
    let (booted, _dir) = boot().await;
    for class in [
        "transient",
        "contract_violation",
        "permission_denied",
        "unavailable",
        "internal",
    ] {
        let (status, answer) = ask(
            &booted,
            RELAY_SHELF,
            RELAY_VERB,
            &json!({ "coded": true, "ask": { "class": class, "values": true } }),
        )
        .await;
        assert_eq!(status, 200, "{class}: {answer}");
        assert_eq!(answer["class"], class, "{class}: {answer}");
        assert_eq!(answer["subclass"], "share_busy", "{class}: {answer}");
        assert_eq!(
            answer["values"],
            json!({ "share": "Music", "minutes": 30 }),
            "{class}: {answer}"
        );
    }
}

#[tokio::test]
async fn a_nest_that_asks_dispatch_reads_what_it_always_did() {
    let _env = ENV_LOCK.lock().await;
    let (booted, _dir) = boot().await;
    for (class, kind) in [
        ("transient", json!("transient")),
        ("contract_violation", json!("permanent")),
        ("permission_denied", json!({ "refused": "share_busy" })),
    ] {
        for values in [false, true] {
            for coded in [false, true] {
                let (status, answer) = ask(
                    &booted,
                    RELAY_SHELF,
                    RELAY_VERB,
                    &json!({
                        "coded": coded,
                        "ask": { "class": class, "values": values },
                    }),
                )
                .await;
                assert_eq!(status, 200, "{class}: {answer}");
                assert_eq!(
                    answer["kind"], kind,
                    "{class} values={values} coded={coded}: {answer}"
                );
            }
        }
    }
}
