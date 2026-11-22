// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! A network subscriber to the happenings hears what its session may
//! read, and nothing else.
//!
//! Each happening reaches a subscriber whose session holds the read
//! scope of what it reports; a plugin's own event reaches a session
//! holding a scope its plugin declares, or `read:plugins` when the
//! plugin declares none. The pins subscribe over the operator
//! WebSocket, through the listener `maybe_boot_https` mounts, as a
//! narrow bearer, as a pair-style bearer, as a reader of subjects
//! alone and as a LAN caller without a bearer, and emit four
//! happenings on the steward's bus: a subject change, a household
//! change, an event of a plugin that declares `voucher_issue`, and an
//! event of a plugin that declares nothing.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::{Happening, HappeningBus};
use evo::household_protection::HouseholdProtectionRuntime;
use evo::persistence::MemoryPersistenceStore;
use evo::projections::ProjectionEngine;
use evo::relations::RelationGraph;
use evo::server::Server;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo::{maybe_boot_https, LanPrivilege};
use evo_auth_bearer::{Capability, CapabilitySet, DEFAULT_TOKEN_TTL_MS};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

/// The boot reads its listener and state directory from the
/// environment, which is process-wide.
static ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

const COUNTER: &str = "org.test.desk.counter";
const COUNTER_SHELF: &str = "desk.counter";
const OTHER: &str = "org.test.desk.other";
const OTHER_SHELF: &str = "desk.other";

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "desk"
family = "domain"
charter = "Two shelves for the happenings-by-reads pins."

[[racks.shelves]]
name = "counter"
shape = 1
description = "A plugin that declares a scope of its own."

[[racks.shelves]]
name = "other"
shape = 1
description = "A plugin that declares no scope."
"#;

fn manifest(name: &str, shelf: &str, verb_capabilities: &str) -> Manifest {
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
request_types = ["ping"]
response_budget_ms = 30000
{verb_capabilities}
"#
    );
    Manifest::from_toml(&toml).expect("manifest must parse")
}

struct Quiet(&'static str);

impl Plugin for Quiet {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: self.0.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec!["ping".to_string()],
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

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move { HealthReport::healthy() }
    }
}

impl Respondent for Quiet {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move { Ok(Response::for_request(req, req.payload.clone())) }
    }
}

struct Booted {
    addr: std::net::SocketAddr,
    ca_pem: String,
    state: Arc<StewardState>,
    issuer: Arc<evo_auth_bearer::BearerTokenIssuer>,
    _handles: evo::https_boot::HttpsBootHandles,
    _server: Arc<Server>,
    _dir: tempfile::TempDir,
}

async fn boot() -> Booted {
    let dir = tempfile::tempdir().expect("tempdir");
    let parent = dir.path().to_path_buf();
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
            "happenings-by-reads",
        )))
        .build()
        .expect("steward state");
    let mut engine = AdmissionEngine::new(
        Arc::clone(&state),
        parent.join("data-root"),
        std::path::PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    );
    engine
        .admit_singleton_respondent(
            Quiet(COUNTER),
            manifest(
                COUNTER,
                COUNTER_SHELF,
                "[capabilities.respondent.verb_capabilities]\n\
                 ping = { kind = \"write\", scope = \"voucher_issue\" }\n",
            ),
        )
        .await
        .expect("admit the counter");
    engine
        .admit_singleton_respondent(
            Quiet(OTHER),
            manifest(OTHER, OTHER_SHELF, ""),
        )
        .await
        .expect("admit the other plugin");
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
    std::env::set_var("EVO_HTTPS_LISTEN_ADDR", "127.0.0.1:0");
    std::env::set_var("EVO_HTTPS_STATE_DIR", &parent);
    std::env::remove_var("EVO_AUTH_TIER");
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
    let ca_pem =
        std::fs::read_to_string(parent.join("https").join("ca.crt")).unwrap();
    Booted {
        addr: handles.server.local_addr,
        ca_pem,
        state,
        issuer: Arc::clone(&handles.issuer),
        _handles: handles,
        _server: server,
        _dir: dir,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Open the operator WebSocket, as `bearer` when given, and subscribe
/// to the happenings. Returns once the subscription is acknowledged.
async fn subscribe(booted: &Booted, bearer: Option<&str>) -> Socket {
    evo_runtime_http::install_crypto_provider();
    let mut req = format!("wss://{}/api/v1/ws", booted.addr)
        .into_client_request()
        .unwrap();
    if let Some(b) = bearer {
        req.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {b}")).unwrap(),
        );
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut booted.ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let (mut socket, _) = connect_async_tls_with_config(
        req,
        None,
        false,
        Some(Connector::Rustls(Arc::new(cfg))),
    )
    .await
    .expect("WebSocket upgrade");
    socket
        .send(Message::Text(
            json!({
                "frame_type": "subscribe",
                "subscription_id": "h",
                "op": "subscribe_happenings",
                "payload": {},
            })
            .to_string(),
        ))
        .await
        .unwrap();
    loop {
        let frame = next_frame(&mut socket, Duration::from_secs(10))
            .await
            .expect("an answer to the subscribe frame");
        match frame["frame_type"].as_str() {
            Some("subscription_ack") => return socket,
            Some("subscription_ended") => panic!("refused: {frame}"),
            _ => continue,
        }
    }
}

async fn next_frame(socket: &mut Socket, within: Duration) -> Option<Value> {
    loop {
        let msg = tokio::time::timeout(within, socket.next()).await.ok()??;
        if let Ok(Message::Text(text)) = msg {
            return serde_json::from_str(&text).ok();
        }
    }
}

/// What a subscriber heard after the four happenings were emitted:
/// each event's `type`, and `event_type` for a plugin's own event.
async fn heard(socket: &mut Socket) -> BTreeSet<String> {
    let mut heard = BTreeSet::new();
    while let Some(frame) = next_frame(socket, Duration::from_millis(750)).await
    {
        if frame["frame_type"] != "subscription_event" {
            continue;
        }
        let event = &frame["event"];
        let name = match event["event_type"].as_str() {
            Some(event_type) => event_type.to_string(),
            None => event["type"].as_str().unwrap_or("?").to_string(),
        };
        heard.insert(name);
    }
    heard
}

fn emit_the_four(booted: &Booted) {
    let at = SystemTime::now();
    let bus = &booted.state.bus;
    bus.emit(Happening::SubjectStateChanged {
        plugin: OTHER.to_string(),
        canonical_id: "subject-1".to_string(),
        subject_type: "track".to_string(),
        prev_state: json!({}),
        new_state: json!({ "playing": true }),
        at,
    });
    bus.emit(Happening::HouseholdProtectionChanged {
        chosen: true,
        level: "open".to_string(),
        lend: false,
        protected_groups: vec![],
        prior_level: None,
    });
    bus.emit(Happening::PluginEvent {
        plugin: COUNTER.to_string(),
        event_type: "voucher_handed".to_string(),
        payload: json!({}),
        at,
    });
    bus.emit(Happening::PluginEvent {
        plugin: OTHER.to_string(),
        event_type: "other_event".to_string(),
        payload: json!({}),
        at,
    });
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[tokio::test]
async fn each_subscriber_hears_what_its_session_may_read() {
    let _env = ENV_LOCK.lock().await;
    let booted = boot().await;
    let mint = |caps: Vec<Capability>| {
        booted
            .issuer
            .issue(CapabilitySet::new(caps), DEFAULT_TOKEN_TTL_MS, now_ms())
            .expect("issue")
            .encode()
    };
    // The front desk: its plugin's scope and nothing of the framework's.
    let desk = mint(vec![Capability::write("voucher_issue")]);
    // A pair bearer: the operator set and every scope the plugins
    // declare.
    let mut operator = evo::https_boot::operator_bootstrap_capability_set()
        .capabilities()
        .to_vec();
    operator.push(Capability::write("voucher_issue"));
    let paired = mint(operator);
    // A reader of subjects alone, as a prompt responder is minted.
    let subjects_only = mint(vec![
        Capability::write("user_interaction_responder"),
        Capability::read("subjects"),
    ]);

    let mut as_desk = subscribe(&booted, Some(&desk)).await;
    let mut as_paired = subscribe(&booted, Some(&paired)).await;
    let mut as_subjects = subscribe(&booted, Some(&subjects_only)).await;
    let mut as_lan = subscribe(&booted, None).await;

    emit_the_four(&booted);

    assert_eq!(heard(&mut as_desk).await, set(&["voucher_handed"]));
    assert_eq!(
        heard(&mut as_paired).await,
        set(&[
            "subject_state_changed",
            "household_protection_changed",
            "voucher_handed",
            "other_event",
        ])
    );
    assert_eq!(
        heard(&mut as_subjects).await,
        set(&["subject_state_changed"])
    );
    // The LAN without a bearer holds the framework's reads, not the
    // counter's own scope.
    assert_eq!(
        heard(&mut as_lan).await,
        set(&[
            "subject_state_changed",
            "household_protection_changed",
            "other_event",
        ])
    );
}

#[tokio::test]
async fn a_bearer_without_read_subjects_may_subscribe() {
    // The subscription used to need `read:subjects`; it is open to any
    // admitted caller, and the stream is what is gated.
    let _env = ENV_LOCK.lock().await;
    let booted = boot().await;
    let desk = booted
        .issuer
        .issue(
            CapabilitySet::new(vec![Capability::write("voucher_issue")]),
            DEFAULT_TOKEN_TTL_MS,
            now_ms(),
        )
        .unwrap()
        .encode();
    let _socket = subscribe(&booted, Some(&desk)).await;
}
