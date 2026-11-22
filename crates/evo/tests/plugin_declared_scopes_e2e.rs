// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! A scope a plugin declares on its verbs, at any rank, is carried by
//! the bearers the steward mints: the unit's first-boot bearer, a pair
//! bearer, and a bearer minted for that scope alone.
//!
//! The plugin is a front desk's counter: `hand` declares
//! `write:voucher_issue`, `offered` declares `read:voucher_issue`,
//! `set_network` declares `write:network_admin`, and `ping` declares
//! nothing. A front desk is minted with `request`, the dispatch key,
//! and `voucher_issue`: it passes the first two and is refused the
//! rest, evo's own plugin reads included. Everything goes through a
//! public path: the steward's local socket, and HTTPS through the
//! listener `maybe_boot_https` mounts.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::client_acl::{ClientAcl, StewardIdentity};
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::household_protection::HouseholdProtectionRuntime;
use evo::persistence::MemoryPersistenceStore;
use evo::projections::ProjectionEngine;
use evo::relations::RelationGraph;
use evo::resolution::ResolutionLedger;
use evo::server::Server;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo::{maybe_boot_https, LanPrivilege};
use evo_auth_bearer::{BearerToken, Capability, CapabilitySet};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// The boot reads its listener and state directory from the
/// environment, which is process-wide.
static ENV_LOCK: std::sync::LazyLock<Mutex<()>> =
    std::sync::LazyLock::new(|| Mutex::new(()));

const PLUGIN: &str = "org.test.desk.counter";
const SHELF: &str = "desk.counter";
const HAND: &str = "hand";
const OFFERED: &str = "offered";
const SET_NETWORK: &str = "set_network";
const PING: &str = "ping";
const PRESEED: &str = "24681357";

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "desk"
family = "domain"
charter = "One-shelf rack for the plugin-declared scope pins."

[[racks.shelves]]
name = "counter"
shape = 1
description = "A front desk's counter."
"#;

fn manifest() -> Manifest {
    let toml = format!(
        r#"
[plugin]
name = "{PLUGIN}"
version = "0.1.0"
contract = 1

[target]
shelf = "{SHELF}"
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
request_types = ["{HAND}", "{OFFERED}", "{SET_NETWORK}", "{PING}"]
response_budget_ms = 30000

[capabilities.respondent.verb_capabilities]
"{HAND}" = {{ kind = "write", scope = "voucher_issue" }}
"{OFFERED}" = {{ kind = "read", scope = "voucher_issue" }}
"{SET_NETWORK}" = {{ kind = "write", scope = "network_admin" }}
"#
    );
    Manifest::from_toml(&toml).expect("counter manifest must parse")
}

/// Answers every verb with its own payload.
struct Counter;

impl Plugin for Counter {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN.to_string(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        HAND.to_string(),
                        OFFERED.to_string(),
                        SET_NETWORK.to_string(),
                        PING.to_string(),
                    ],
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

impl Respondent for Counter {
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
    https_dir: PathBuf,
    socket_path: PathBuf,
    _handles: evo::https_boot::HttpsBootHandles,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn boot(dir: &Path) -> Booted {
    let parent = dir.to_path_buf();
    let socket_path = parent.join("evo.sock");
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
            "plugin-declared-scopes",
        )))
        .build()
        .expect("steward state");
    let mut engine = AdmissionEngine::new(
        Arc::clone(&state),
        parent.join("data-root"),
        PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    );
    engine
        .admit_singleton_respondent(Counter, manifest())
        .await
        .expect("admit the counter");
    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let household = Arc::new(HouseholdProtectionRuntime::load(
        parent.clone(),
        None,
        evo::https_boot::operator_bootstrap_capability_set(),
    ));
    let server = Arc::new(
        Server::with_acl(
            socket_path.clone(),
            Arc::clone(engine.router()),
            Arc::clone(&state),
            projections,
            Arc::new(ClientAcl::default()),
            StewardIdentity::current(),
            Arc::new(ResolutionLedger::new()),
            evo::catalogue::CatalogueSource::Configured,
            evo::time_trust::new_shared(),
            evo::time_trust::new_shared_last_step(),
            false,
        )
        .with_household_protection(household)
        .with_auth(None, Arc::new(evo::auth::AuthSessionStore::with_defaults()))
        .with_bootstrap_pair_preseed(PRESEED.to_string(), "test".to_string()),
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
    // What `run()` does after the HTTPS boot.
    let _ = state.bearer_token_issuer.set(Arc::clone(&handles.issuer));
    let _ = state
        .credential_store
        .set(Arc::clone(&handles.credential_store));
    let _ = state
        .revocation_list
        .set(Arc::clone(&handles.revocation_list));
    let _ = state.https_dir.set(handles.https_dir.clone());

    let (shutdown, rx) = tokio::sync::oneshot::channel::<()>();
    let unix = Arc::clone(&server);
    tokio::spawn(async move {
        let _ = unix
            .run(async move {
                let _ = rx.await;
            })
            .await;
    });
    for _ in 0..200 {
        if UnixStream::connect(&socket_path).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let https_dir = parent.join("https");
    let ca_pem = std::fs::read_to_string(https_dir.join("ca.crt")).unwrap();
    Booted {
        addr: handles.server.local_addr,
        ca_pem,
        https_dir,
        socket_path,
        _handles: handles,
        _shutdown: shutdown,
    }
}

fn caps_of(encoded: &str) -> CapabilitySet {
    BearerToken::decode(encoded)
        .expect("bearer decodes")
        .capabilities
}

/// One REST call from loopback, as `bearer` when given.
async fn post(
    booted: &Booted,
    op: &str,
    body: Value,
    bearer: Option<&str>,
) -> (u16, Value) {
    let client = reqwest::Client::builder()
        .add_root_certificate(
            reqwest::Certificate::from_pem(booted.ca_pem.as_bytes()).unwrap(),
        )
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .build()
        .unwrap();
    let mut req = client
        .post(format!("https://{}/api/v1/{op}", booted.addr))
        .json(&body);
    if let Some(bearer) = bearer {
        req = req.bearer_auth(bearer);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// One GET of an op from loopback, as `bearer` when given.
async fn get(booted: &Booted, op: &str, bearer: Option<&str>) -> u16 {
    let client = reqwest::Client::builder()
        .add_root_certificate(
            reqwest::Certificate::from_pem(booted.ca_pem.as_bytes()).unwrap(),
        )
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(true)
        .build()
        .unwrap();
    let mut req = client.get(format!("https://{}/api/v1/{op}", booted.addr));
    if let Some(bearer) = bearer {
        req = req.bearer_auth(bearer);
    }
    req.send().await.unwrap().status().as_u16()
}

/// Ask `verb` on the counter with no bearer, from loopback.
async fn ask_anonymously(booted: &Booted, verb: &str) -> (u16, Value) {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    post(
        booted,
        "request",
        json!({
            "shelf": SHELF,
            "request_type": verb,
            "payload_b64": B64.encode(b"{}"),
        }),
        None,
    )
    .await
}

/// Mint a bearer on the local socket, narrowed to `scopes`.
async fn mint_on_the_socket(booted: &Booted, scopes: &[&str]) -> String {
    let mut stream = UnixStream::connect(&booted.socket_path).await.unwrap();
    let granted = unix_ask(
        &mut stream,
        json!({"op": "negotiate", "capabilities": ["plugins_admin"]}),
    )
    .await;
    assert_eq!(granted["granted"], json!(["plugins_admin"]), "{granted}");
    let minted = unix_ask(
        &mut stream,
        json!({
            "op": "mint_bearer_token",
            "reason": "front desk",
            "capabilities": scopes,
        }),
    )
    .await;
    minted["token"]
        .as_str()
        .unwrap_or_else(|| panic!("{minted}"))
        .to_string()
}

fn refused_by_scope(status: u16, body: &Value) -> bool {
    status == 403
        && body
            .pointer("/error/details/subclass")
            .and_then(Value::as_str)
            == Some("verb_capability_scope_not_granted")
}

/// Ask `verb` on the counter as `bearer`.
async fn ask(booted: &Booted, bearer: &str, verb: &str) -> (u16, Value) {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    post(
        booted,
        "request",
        json!({
            "shelf": SHELF,
            "request_type": verb,
            "payload_b64": B64.encode(b"{}"),
        }),
        Some(bearer),
    )
    .await
}

async fn unix_ask(stream: &mut UnixStream, req: Value) -> Value {
    let body = serde_json::to_vec(&req).unwrap();
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&body).await.unwrap();
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await.unwrap();
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut buf).await.unwrap();
    serde_json::from_slice(&buf).unwrap()
}

/// The bearer passes both of the counter's voucher verbs.
async fn assert_passes_the_voucher_verbs(booted: &Booted, bearer: &str) {
    for verb in [HAND, OFFERED] {
        let (status, body) = ask(booted, bearer, verb).await;
        assert_eq!(status, 200, "{verb}: {body}");
    }
}

#[tokio::test]
async fn the_first_boot_bearer_carries_what_the_plugins_declare() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let booted = boot(dir.path()).await;
    let bootstrap =
        std::fs::read_to_string(booted.https_dir.join("bootstrap.token"))
            .expect("the first boot writes its bearer");
    let caps = caps_of(bootstrap.trim());
    assert!(
        caps.capabilities()
            .contains(&Capability::write("voucher_issue")),
        "{caps:?}"
    );
    assert_passes_the_voucher_verbs(&booted, bootstrap.trim()).await;
}

#[tokio::test]
async fn a_pair_bearer_carries_what_the_plugins_declare() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let booted = boot(dir.path()).await;
    let (status, body) = post(
        &booted,
        "pair_complete",
        json!({ "pair_id": "bootstrap", "code": PRESEED }),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let paired = body["token"].as_str().expect("paired bearer").to_string();
    assert!(caps_of(&paired)
        .capabilities()
        .contains(&Capability::write("voucher_issue")));
    assert_passes_the_voucher_verbs(&booted, &paired).await;
}

#[tokio::test]
async fn a_narrow_role_reaches_its_verbs_and_none_of_evos_reads() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let booted = boot(dir.path()).await;
    let desk = mint_on_the_socket(&booted, &["request", "voucher_issue"]).await;
    let caps = caps_of(&desk);
    let scopes: std::collections::BTreeSet<&str> =
        caps.capabilities().iter().map(|c| c.scope()).collect();
    assert_eq!(
        scopes,
        ["request", "voucher_issue"].into_iter().collect(),
        "{caps:?}"
    );
    assert!(
        caps.capabilities()
            .contains(&Capability::write("voucher_issue")),
        "{caps:?}"
    );
    assert_passes_the_voucher_verbs(&booted, &desk).await;
    for verb in [SET_NETWORK, PING] {
        let (status, body) = ask(&booted, &desk, verb).await;
        assert!(refused_by_scope(status, &body), "{verb}: {status} {body}");
    }
    for op in ["list_plugins", "describe_ui_stockings"] {
        assert_eq!(get(&booted, op, Some(&desk)).await, 403, "{op}");
    }
    assert_eq!(
        get(&booted, "describe_capabilities", Some(&desk)).await,
        200,
        "discovery stays open"
    );
}

#[tokio::test]
async fn a_bearer_without_request_does_not_dispatch() {
    // `plugins` used to be the dispatch key; `request` is.
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let booted = boot(dir.path()).await;
    let old_desk =
        mint_on_the_socket(&booted, &["plugins", "voucher_issue"]).await;
    for verb in [HAND, PING] {
        let (status, _) = ask(&booted, &old_desk, verb).await;
        assert_eq!(status, 403, "{verb}");
    }
}

#[tokio::test]
async fn the_home_player_and_its_bearers_reach_undeclared_verbs() {
    let _env = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let booted = boot(dir.path()).await;
    let (status, body) = ask_anonymously(&booted, PING).await;
    assert_eq!(status, 200, "the LAN without a bearer: {body}");
    let bootstrap =
        std::fs::read_to_string(booted.https_dir.join("bootstrap.token"))
            .unwrap();
    let (status, body) = ask(&booted, bootstrap.trim(), PING).await;
    assert_eq!(status, 200, "the first-boot bearer: {body}");
    let (status, body) = post(
        &booted,
        "pair_complete",
        json!({ "pair_id": "bootstrap", "code": PRESEED }),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let paired = body["token"].as_str().expect("paired bearer").to_string();
    let (status, body) = ask(&booted, &paired, PING).await;
    assert_eq!(status, 200, "a pair bearer: {body}");
}
