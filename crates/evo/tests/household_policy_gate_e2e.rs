// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The household-protection dispatch gate, over real HTTPS.
//!
//! The stamp half is proven in `evo-runtime-http`'s recovery
//! contract: a LAN principal reaches the privileged writers only
//! when a runtime exists to police them. That is admission. This
//! file proves the other half — that a policy protecting a group
//! actually refuses the write, with the subclass the operator
//! surface classifies on.
//!
//! Everything here goes through a public path:
//!
//! - the verb is admitted by `AdmissionEngine::admit_singleton_respondent`,
//!   so its capability comes from a real manifest rather than a
//!   hand-built enforcement policy;
//! - the request is real HTTPS through the listener `maybe_boot_https`
//!   mounts;
//! - the principal is minted by the middleware (anonymous LAN-trust)
//!   or is a genuine issuer bearer recorded in the kiosk registry.
//!
//! No `Principal` is fabricated. A fabricated one would prove the
//! classifier and not the refuse.

use std::future::Future;
use std::sync::{Arc, LazyLock};

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::household_protection::{
    GroupTable, HouseholdGroupTable, HouseholdProtectionPolicy,
    HouseholdProtectionRuntime, ProtectionLevel,
};
use evo::maybe_boot_https;
use evo::persistence::MemoryPersistenceStore;
use evo::projections::ProjectionEngine;
use evo::relations::RelationGraph;
use evo::server::Server;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde_json::Value;
use tokio::sync::Mutex;

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const PLUGIN: &str = "org.test.household.panel";
const SHELF: &str = "panel.display";
/// A single mutating verb declaring `write:system_admin`, so the
/// dispatcher resolves a real scope and reaches `is_scope_protected`.
const GATED_VERB: &str = "set_cursor";
const GATED_SCOPE: &str = "system_admin";
/// Playback transport. No verb capability — the gate must never
/// lock browse / play / pause / skip / volume, including under lend.
const PLAY_VERB: &str = "play";

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "panel"
family = "domain"
charter = "One-shelf rack for the household-protection gate test."

[[racks.shelves]]
name = "display"
shape = 1
description = "Display shelf carrying one mutating verb."
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
request_types = ["{GATED_VERB}", "{PLAY_VERB}"]
response_budget_ms = 30000

[capabilities.respondent.verb_capabilities.{GATED_VERB}]
kind = "write"
scope = "{GATED_SCOPE}"
"#
    );
    Manifest::from_toml(&toml).expect("gate manifest must parse")
}

/// The distribution table under test. Opaque keys, exactly as the
/// steward sees them — this stands in for evo-device-audio's table
/// and the steward learns nothing from it but scope names.
struct FixtureTable;

impl HouseholdGroupTable for FixtureTable {
    fn group_keys(&self) -> Vec<String> {
        vec!["system".to_owned(), "network".to_owned()]
    }
    fn scopes_for_group(&self, key: &str) -> Vec<String> {
        match key {
            "system" => vec![GATED_SCOPE.to_owned()],
            "network" => vec!["network_admin".to_owned()],
            _ => Vec::new(),
        }
    }
    fn groups_for_level(&self, level: ProtectionLevel) -> Vec<String> {
        match level {
            ProtectionLevel::Open => Vec::new(),
            _ => vec!["system".to_owned(), "network".to_owned()],
        }
    }
}

/// Admits the gated verb. If a request reaches it, the gate let it
/// through — which is how the control tells "admitted" from
/// "refused" by evidence rather than by response shape.
struct PanelRespondent {
    loaded: bool,
}

impl Plugin for PanelRespondent {
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
                        GATED_VERB.to_string(),
                        PLAY_VERB.to_string(),
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
        async move {
            self.loaded = true;
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

impl Respondent for PanelRespondent {
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
    server: Arc<Server>,
    /// The very store the steward validates step-up tokens against,
    /// so the widen-with-sitting case can mint a genuine sitting
    /// rather than assert around one.
    auth: Arc<evo::auth::AuthSessionStore>,
    /// Held by the test so the kiosk case can reach the issuer and
    /// the registry without widening `Server`'s surface.
    state: Arc<StewardState>,
}

/// Boot a steward with the gated verb admitted through the public
/// admission path and `policy` in force.
async fn boot(
    policy: HouseholdProtectionPolicy,
) -> (Booted, tempfile::TempDir) {
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
            "household-gate",
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
            PanelRespondent { loaded: false },
            manifest(),
        )
        .await
        .expect("admit panel respondent");

    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let router = Arc::clone(engine.router());

    let table: GroupTable = Arc::new(FixtureTable);
    let runtime = Arc::new(HouseholdProtectionRuntime::load(
        parent.clone(),
        Some(table),
        evo::https_boot::operator_bootstrap_capability_set(),
    ));
    // `apply` is the set path and forces `chosen = true`. A policy
    // that has not been chosen must reach the runtime unapplied, or
    // the first-start case would never actually be first start.
    if policy.chosen {
        runtime.apply(policy).expect("policy persists");
    }

    let auth = Arc::new(evo::auth::AuthSessionStore::with_defaults());
    let server = Arc::new(
        Server::new(
            parent.join("unused.sock"),
            router,
            Arc::clone(&state),
            projections,
        )
        .with_household_protection(Arc::clone(&runtime))
        .with_auth(None, Arc::clone(&auth)),
    );

    let handles = maybe_boot_https(
        Arc::clone(&server),
        &parent.join("persistence.sqlite"),
        None,
        None,
    )
    .await
    .expect("maybe_boot_https Ok")
    .expect("HTTPS listener mounted");

    // `run()` attaches the issuer to steward state after HTTPS boot
    // (lib.rs, "bearer-token issuer already attached" block). This
    // harness calls maybe_boot_https directly, so it performs the
    // same wiring — the kiosk case needs the issuer the validator
    // verifies against, not a second one.
    let _ = state.bearer_token_issuer.set(Arc::clone(&handles.issuer));

    let addr = handles.server.local_addr;
    let ca_pem =
        std::fs::read_to_string(parent.join("https").join("ca.crt")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (
        Booted {
            addr,
            ca_pem,
            server,
            state,
            auth,
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

fn request_body() -> Value {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    serde_json::json!({
        "shelf": SHELF,
        "request_type": GATED_VERB,
        "payload_b64": B64.encode(b"{}"),
    })
}

fn play_body() -> Value {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    serde_json::json!({
        "shelf": SHELF,
        "request_type": PLAY_VERB,
        "payload_b64": B64.encode(b"{}"),
    })
}

/// Pull the refusal subclass out of whatever envelope came back.
fn subclass_of(body: &Value) -> Option<String> {
    for ptr in [
        "/error/details/subclass",
        "/error/subclass",
        "/subclass",
        "/body/error/details/subclass",
    ] {
        if let Some(s) = body.pointer(ptr).and_then(|v| v.as_str()) {
            return Some(s.to_owned());
        }
    }
    None
}

fn lending_policy() -> HouseholdProtectionPolicy {
    HouseholdProtectionPolicy {
        chosen: true,
        level: ProtectionLevel::Open,
        lend: true,
        protected_groups: vec!["system".to_owned(), "network".to_owned()],
        prior_level: None,
    }
}

fn strict_policy() -> HouseholdProtectionPolicy {
    HouseholdProtectionPolicy {
        chosen: true,
        level: ProtectionLevel::Strict,
        ..Default::default()
    }
}

fn open_policy() -> HouseholdProtectionPolicy {
    HouseholdProtectionPolicy {
        chosen: true,
        level: ProtectionLevel::Open,
        ..Default::default()
    }
}

#[tokio::test]
async fn lend_refuses_a_protected_write_on_a_lan_trust_principal() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(lending_policy()).await;
    let client = client_with_ca(&booted.ca_pem);

    // Anonymous, from loopback. The middleware mints the LAN-trust
    // principal itself.
    let resp = client
        .post(format!("https://{}/api/v1/request", booted.addr))
        .json(&request_body())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);

    assert_eq!(
        subclass_of(&body).as_deref(),
        Some("household_policy_locked"),
        "lend must refuse a protected write on LAN-trust with the \
         policy subclass, not a capability miss. status={status} \
         body={body}"
    );
}

#[tokio::test]
async fn lend_still_admits_playback() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(lending_policy()).await;
    let client = client_with_ca(&booted.ca_pem);

    let resp = client
        .post(format!("https://{}/api/v1/request", booted.addr))
        .json(&play_body())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);

    assert_eq!(
        status, 200,
        "lend must not lock play / pause / skip / volume. \
         status={status} body={body}"
    );
    assert!(
        subclass_of(&body).is_none(),
        "playback must not carry household_policy_locked. body={body}"
    );
}

#[tokio::test]
async fn pair_begin_still_works_under_lend() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(lending_policy()).await;
    let (status, body) = call(
        &booted,
        serde_json::json!({
            "op": "pair_begin",
            "device_hint": "blast-radius",
        }),
    )
    .await;
    assert_eq!(
        status, 200,
        "pair mint for an API slip must still work under lend. \
         body={body}"
    );
    assert!(
        body["pair_id"].as_str().is_some(),
        "pair_begin must return a pair_id. body={body}"
    );
}

#[tokio::test]
async fn lend_refuses_a_protected_write_on_a_kiosk_bearer_without_remint() {
    let _guard = ENV_LOCK.lock().await;
    // The panel is minted BEFORE the operator lends the box: boot
    // open, mint, then move the policy under the live token. Nothing
    // re-mints, which is the requirement — lending has to lock a
    // panel that is already open.
    let (booted, _dir) = boot(open_policy()).await;
    let client = client_with_ca(&booted.ca_pem);

    let issuer = booted
        .state
        .bearer_token_issuer
        .get()
        .expect("issuer wired by boot")
        .clone();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let token = issuer
        .issue(
            evo::https_boot::operator_bootstrap_capability_set(),
            600_000,
            now_ms,
        )
        .expect("kiosk mint");
    // Exactly what the kiosk socket handler does after a mint.
    booted
        .state
        .kiosk_bearers
        .record(&token.id, token.expires_at_ms);
    let encoded = token.encode();

    booted
        .server
        .household_protection()
        .expect("runtime attached")
        .apply(lending_policy())
        .expect("policy applies");

    let resp = client
        .post(format!("https://{}/api/v1/request", booted.addr))
        .bearer_auth(&encoded)
        .json(&request_body())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);

    assert_eq!(
        subclass_of(&body).as_deref(),
        Some("household_policy_locked"),
        "a kiosk bearer minted before the lend must still be refused, \
         with no remint. status={status} body={body}"
    );
}

#[tokio::test]
async fn open_policy_admits_the_same_write() {
    let _guard = ENV_LOCK.lock().await;
    // The control. Same verb, same principal shape, policy open: the
    // refuse above cannot be "verb missing" or "stamp empty".
    let (booted, _dir) = boot(open_policy()).await;
    let client = client_with_ca(&booted.ca_pem);

    let resp = client
        .post(format!("https://{}/api/v1/request", booted.addr))
        .json(&request_body())
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap_or(Value::Null);

    assert_ne!(
        subclass_of(&body).as_deref(),
        Some("household_policy_locked"),
        "an open policy must not lock anything. body={body}"
    );
}

// ---------------------------------------------------------------
// household_protection_get / _set, and the change happening.
// ---------------------------------------------------------------

async fn call(booted: &Booted, body: Value) -> (u16, Value) {
    let client = client_with_ca(&booted.ca_pem);
    let op = body["op"].as_str().unwrap().to_owned();
    // Mirrors derive_method: read ops are GET, write ops POST.
    let url = format!("https://{}/api/v1/{op}", booted.addr);
    let req = if op.ends_with("_get") {
        client.get(&url)
    } else {
        client.post(&url).json(&body)
    };
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let value: Value = resp.json().await.unwrap_or(Value::Null);
    (status, value)
}

fn get_body() -> Value {
    serde_json::json!({ "op": "household_protection_get" })
}

fn set_body(level: &str, lend: bool) -> Value {
    serde_json::json!({
        "op": "household_protection_set",
        "level": level,
        "lend": lend,
    })
}

#[tokio::test]
async fn first_start_get_reports_unchosen_and_open() {
    let _guard = ENV_LOCK.lock().await;
    // No policy file: the box is a player while they read.
    let (booted, _dir) = boot(HouseholdProtectionPolicy::default()).await;
    let (status, body) = call(&booted, get_body()).await;
    assert_eq!(status, 200, "get must never step-up. body={body}");
    assert_eq!(body["household_protection"], serde_json::json!(true));
    assert_eq!(body["chosen"], serde_json::json!(false));
    assert_eq!(body["level"], serde_json::json!("open"));
    assert_eq!(body["lend"], serde_json::json!(false));
    assert_eq!(body["protected_groups"], serde_json::json!([]));
    assert_eq!(body["prior_level"], Value::Null);
    // Catalog comes from the hooked table, so the surface reads once.
    assert!(body["catalog"]["levels"].is_array());
    assert!(body["catalog"]["groups"].is_array());
}

#[tokio::test]
async fn set_open_at_first_start_needs_no_sitting() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(HouseholdProtectionPolicy::default()).await;
    let (status, body) = call(&booted, set_body("open", false)).await;
    assert_eq!(
        status, 200,
        "first-start set must succeed without step-up. body={body}"
    );
    assert_eq!(body["chosen"], serde_json::json!(true));
    assert_eq!(body["level"], serde_json::json!("open"));
}

#[tokio::test]
async fn lend_with_no_marks_persists_the_low_floor() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(HouseholdProtectionPolicy::default()).await;
    let (status, body) = call(&booted, set_body("open", true)).await;
    assert_eq!(status, 200, "lend must apply. body={body}");
    let marks = body["protected_groups"].as_array().unwrap();
    assert!(
        !marks.is_empty(),
        "lend + empty marks must never be stored; the low floor \
         fills it. body={body}"
    );
}

#[tokio::test]
async fn widening_without_a_sitting_is_refused() {
    let _guard = ENV_LOCK.lock().await;
    // Lending, then trying to lift it with no step-up token: a guest
    // must not unlock the panel from the panel.
    let (booted, _dir) = boot(lending_policy()).await;
    let (status, body) = call(&booted, set_body("open", false)).await;
    assert_ne!(
        status, 200,
        "lifting a lend without a sitting must not succeed. \
         body={body}"
    );
    assert_eq!(
        subclass_of(&body).as_deref(),
        Some("step_up_required"),
        "widening without a sitting must ask for the device \
         password. body={body}"
    );
}

#[tokio::test]
async fn narrowing_never_needs_a_sitting() {
    let _guard = ENV_LOCK.lock().await;
    // The mirror of the case above: handing the box to a guest must
    // never be blocked on a ceremony.
    let (booted, _dir) = boot(open_policy()).await;
    let (status, body) = call(&booted, set_body("strict", false)).await;
    assert_eq!(
        status, 200,
        "narrowing must never require step-up. body={body}"
    );
    assert_eq!(body["level"], serde_json::json!("strict"));
}

#[tokio::test]
async fn corrupt_policy_file_returns_to_first_start() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, dir) = boot(open_policy()).await;
    // Scribble over the persisted policy and rebuild the runtime the
    // way boot does; the discarded file must land on first start,
    // not on a locked box with no door.
    std::fs::write(dir.path().join("household-protection.json"), b"{ not json")
        .unwrap();
    let reloaded = HouseholdProtectionRuntime::load(
        dir.path().to_path_buf(),
        Some(Arc::new(FixtureTable) as GroupTable),
        evo::https_boot::operator_bootstrap_capability_set(),
    );
    let snap = reloaded.snapshot();
    assert!(!snap.chosen, "corrupt file must return to first start");
    assert_eq!(snap.level, ProtectionLevel::Open);
    drop(booted);
}

#[tokio::test]
async fn a_successful_set_emits_the_change_happening() {
    let _guard = ENV_LOCK.lock().await;
    // The bus line is not proven until something observes it. One
    // signal is what lets the panel and a LAN browser refresh
    // without polling, so it is worth a test of its own.
    let (booted, _dir) = boot(HouseholdProtectionPolicy::default()).await;
    let mut rx = booted.state.bus.subscribe();

    let (status, body) = call(&booted, set_body("standard", false)).await;
    assert_eq!(status, 200, "set must succeed. body={body}");

    // Drain until the household event arrives; boot traffic may put
    // other happenings on the bus first.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(5);
    let observed = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "no household_protection_changed observed on the bus"
        );
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            rx.recv(),
        )
        .await
        {
            Ok(Ok(
                evo::happenings::Happening::HouseholdProtectionChanged {
                    chosen,
                    level,
                    lend,
                    protected_groups,
                    prior_level,
                },
            )) => break (chosen, level, lend, protected_groups, prior_level),
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => continue,
        }
    };

    let (chosen, level, lend, groups, prior) = observed;
    assert!(chosen, "post-write chosen must be true");
    assert_eq!(level, "standard", "post-write level, not the request");
    assert!(!lend);
    assert!(prior.is_none());
    // Core fields only. The catalog is deliberately not on the
    // happening — a surface re-uses the one from its last get.
    let wire = serde_json::to_value(
        evo::happenings::Happening::HouseholdProtectionChanged {
            chosen,
            level: level.clone(),
            lend,
            protected_groups: groups.clone(),
            prior_level: prior.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        wire["type"],
        serde_json::json!("household_protection_changed")
    );
    assert!(
        wire.get("catalog").is_none(),
        "the happening must not carry the catalog. wire={wire}"
    );
}

#[tokio::test]
async fn widening_with_a_sitting_succeeds_and_restores_prior_level() {
    let _guard = ENV_LOCK.lock().await;
    // Start standard, then lend: prior_level records where to
    // return to. Lifting the lend is a widen, so it needs the
    // device password — with one, it must succeed and land back on
    // the level the operator was on, not on a second invention.
    let standard = HouseholdProtectionPolicy {
        chosen: true,
        level: ProtectionLevel::Standard,
        ..Default::default()
    };
    let (booted, _dir) = boot(standard).await;

    let (status, body) = call(&booted, set_body("standard", true)).await;
    assert_eq!(status, 200, "lending must not need a sitting. body={body}");
    assert_eq!(body["lend"], serde_json::json!(true));
    assert_eq!(
        body["prior_level"],
        serde_json::json!("standard"),
        "raising lend must record where to return to. body={body}"
    );

    // A genuine sitting in the very store the steward validates
    // against — not a fabricated token.
    // The HTTPS dispatcher's synthetic connection reports uid 0, and
    // validate_step_up_for_operation binds the sitting to the peer
    // uid — so the sitting must be issued for that peer, not an
    // arbitrary one.
    // The caller here is the anonymous LAN-trust principal, which
    // carries the fixed LAN-trust token id. A sitting belongs to the
    // caller that opened it, so it binds to that bearer.
    let session = booted.auth.issue(
        evo::auth::VerifiedPrincipal {
            username: "operator".to_owned(),
            uid: Some(0),
        },
        evo::auth::caller_identity(
            Some(0),
            Some(evo_runtime_http::middleware::LAN_TRUST_TOKEN_ID),
        ),
        None,
    );

    let client = client_with_ca(&booted.ca_pem);
    let resp = client
        .post(format!(
            "https://{}/api/v1/household_protection_set",
            booted.addr
        ))
        .json(&serde_json::json!({
            "op": "household_protection_set",
            "level": "standard",
            "lend": false,
            "step_up_token": session.token,
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);

    assert_eq!(
        status, 200,
        "widening with a live sitting must succeed. body={body}"
    );
    assert_eq!(body["lend"], serde_json::json!(false));
    assert_eq!(
        body["level"],
        serde_json::json!("standard"),
        "unlock returns to the level the operator was on. body={body}"
    );
    assert_eq!(
        body["prior_level"],
        Value::Null,
        "prior_level clears once the overlay is lifted. body={body}"
    );
}

#[tokio::test]
async fn unlock_omits_level_without_a_sitting_is_step_up() {
    let _guard = ENV_LOCK.lock().await;
    // The glass Stop-lending button sends {lend:false} and omits
    // level. That is unlock, not a parse error. Without a sitting
    // it must still be step_up_required — a guest must not lift
    // the overlay from the panel.
    let (booted, _dir) = boot(lending_policy()).await;
    let (status, body) = call(
        &booted,
        serde_json::json!({
            "op": "household_protection_set",
            "lend": false,
        }),
    )
    .await;
    assert_ne!(
        status, 200,
        "lifting a lend without a sitting must not succeed. \
         body={body}"
    );
    assert_eq!(
        subclass_of(&body).as_deref(),
        Some("step_up_required"),
        "omit-level unlock is the same widen as an explicit \
         unlock. body={body}"
    );
    let wire = body.to_string();
    assert!(
        !wire.contains("missing field"),
        "omitted level must not be a parse error. body={body}"
    );
}

#[tokio::test]
async fn unlock_omits_level_with_a_sitting_restores_prior_level() {
    let _guard = ENV_LOCK.lock().await;
    // Same sitting as widening_with_a_sitting_succeeds, but the
    // body is what the glass button actually sends: lend false,
    // no level. The steward restores prior_level.
    let standard = HouseholdProtectionPolicy {
        chosen: true,
        level: ProtectionLevel::Standard,
        ..Default::default()
    };
    let (booted, _dir) = boot(standard).await;

    let (status, body) = call(&booted, set_body("standard", true)).await;
    assert_eq!(status, 200, "lending must not need a sitting. body={body}");
    assert_eq!(body["prior_level"], serde_json::json!("standard"));

    // The caller here is the anonymous LAN-trust principal, which
    // carries the fixed LAN-trust token id. A sitting belongs to the
    // caller that opened it, so it binds to that bearer.
    let session = booted.auth.issue(
        evo::auth::VerifiedPrincipal {
            username: "operator".to_owned(),
            uid: Some(0),
        },
        evo::auth::caller_identity(
            Some(0),
            Some(evo_runtime_http::middleware::LAN_TRUST_TOKEN_ID),
        ),
        None,
    );

    let client = client_with_ca(&booted.ca_pem);
    let resp = client
        .post(format!(
            "https://{}/api/v1/household_protection_set",
            booted.addr
        ))
        .json(&serde_json::json!({
            "op": "household_protection_set",
            "lend": false,
            "step_up_token": session.token,
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);

    assert_eq!(
        status, 200,
        "omit-level unlock with a live sitting must succeed. \
         body={body}"
    );
    assert_eq!(body["lend"], serde_json::json!(false));
    assert_eq!(
        body["level"],
        serde_json::json!("standard"),
        "omit-level unlock must restore prior_level, not invent \
         one. body={body}"
    );
    assert_eq!(body["prior_level"], Value::Null);
}

// ---------------------------------------------------------------
// A step-up sitting binds to `caller_identity`, not to the
// transport.
//
// Two paired browsers are two identities: a sitting opened by one
// does not admit the other. A sitting bound to a Unix peer uid
// belongs to the Unix socket and admits no HTTPS caller.
// ---------------------------------------------------------------

/// Mint a pair-style bearer: operator capabilities, NOT recorded in
/// the kiosk registry. Two of these are two paired browsers.
fn mint_pair_bearer(booted: &Booted) -> (String, String) {
    let issuer = booted
        .state
        .bearer_token_issuer
        .get()
        .expect("issuer wired by boot")
        .clone();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let token = issuer
        .issue(
            evo::https_boot::operator_bootstrap_capability_set(),
            600_000,
            now_ms,
        )
        .expect("pair mint");
    (token.id.clone(), token.encode())
}

async fn widen_as(
    booted: &Booted,
    bearer: &str,
    step_up_token: &str,
) -> (u16, Value) {
    let client = client_with_ca(&booted.ca_pem);
    let resp = client
        .post(format!(
            "https://{}/api/v1/household_protection_set",
            booted.addr
        ))
        .bearer_auth(bearer)
        .json(&serde_json::json!({
            "op": "household_protection_set",
            "level": "open",
            "lend": false,
            "step_up_token": step_up_token,
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn a_sitting_does_not_carry_to_a_second_paired_browser() {
    let _guard = ENV_LOCK.lock().await;
    // Two paired browsers. One sitting, which the second never
    // obtained. The second must still be asked for the password.
    let (booted, _dir) = boot(lending_policy()).await;

    let (browser_a_id, _browser_a) = mint_pair_bearer(&booted);
    let (_browser_b_id, browser_b) = mint_pair_bearer(&booted);

    // A genuine sitting in the very store the steward validates
    // against, bound to browser A — the browser that performed the
    // ceremony.
    let session = booted.auth.issue(
        evo::auth::VerifiedPrincipal {
            username: "operator".to_owned(),
            uid: Some(0),
        },
        evo::auth::caller_identity(Some(0), Some(&browser_a_id)),
        None,
    );

    let (status, body) = widen_as(&booted, &browser_b, &session.token).await;

    assert_ne!(
        status, 200,
        "a second paired browser must not inherit a sitting it never \
         opened. status={status} body={body}"
    );
    assert_eq!(
        subclass_of(&body).as_deref(),
        Some("step_up_required"),
        "the second browser must be asked for the device password. \
         body={body}"
    );
}

#[tokio::test]
async fn a_uid_bound_sitting_never_admits_an_https_caller() {
    let _guard = ENV_LOCK.lock().await;
    // The same fact stated as the rule rather than the scenario: a
    // sitting bound to a PEERCRED uid belongs to the Unix socket.
    // No browser may present it, whichever browser it is.
    let (booted, _dir) = boot(lending_policy()).await;
    let (_id, browser) = mint_pair_bearer(&booted);
    let session = booted.auth.issue(
        evo::auth::VerifiedPrincipal {
            username: "operator".to_owned(),
            uid: Some(0),
        },
        // A Unix-shaped identity, deliberately.
        evo::auth::caller_identity(Some(0), None),
        None,
    );
    let (status, body) = widen_as(&booted, &browser, &session.token).await;
    assert_eq!(
        subclass_of(&body).as_deref(),
        Some("step_up_required"),
        "a uid-bound sitting is not a browser's sitting. \
         status={status} body={body}"
    );
}

// ---------------------------------------------------------------
// The household gate reaches both dispatch paths.
//
// A plugin verb and a framework wire op are two dispatch paths to
// the same device. The policy belongs to the device, so a scope
// the operator protected is protected on both. Every non-empty
// HTTPS/WS token id is bound, pair included; the Unix socket
// carries no token id and stays operator-local.
//
// A live step-up session for the calling identity admits the
// write without changing the level.
// ---------------------------------------------------------------

/// A framework wire op whose declared scope the fixture table
/// protects (`write:system_admin`). Not a plugin verb — it never
/// reaches the plugin router, so a gate that lives only on that
/// path cannot see it.
const FRAMEWORK_OP: &str = "set_device_display_name";

fn framework_protected_body() -> Value {
    serde_json::json!({
        "op": FRAMEWORK_OP,
        "display_name": "listening room",
    })
}

/// Dispatch as `bearer`. The REST projection derives the method
/// from the op id — `set_*` is a PUT, everything else here a POST —
/// so the test speaks the same shape a browser does.
async fn post_as(
    booted: &Booted,
    bearer: &str,
    op: &str,
    body: Value,
) -> (u16, Value) {
    let client = client_with_ca(&booted.ca_pem);
    let url = format!("https://{}/api/v1/{op}", booted.addr);
    let builder = if op.starts_with("set_") {
        client.put(url)
    } else {
        client.post(url)
    };
    let resp = builder
        .bearer_auth(bearer)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let out: Value = resp.json().await.unwrap_or(Value::Null);
    (status, out)
}

#[tokio::test]
async fn strict_refuses_a_plugin_write_on_a_pair_bearer() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(open_policy()).await;
    let (_id, pair) = mint_pair_bearer(&booted);
    booted
        .server
        .household_protection()
        .expect("runtime attached")
        .apply(strict_policy())
        .expect("policy applies");

    let (status, body) =
        post_as(&booted, &pair, "request", request_body()).await;
    assert_eq!(
        subclass_of(&body).as_deref(),
        Some("household_policy_locked"),
        "a paired browser is bound by the policy it chose. \
         status={status} body={body}"
    );
}

#[tokio::test]
async fn strict_refuses_a_framework_op_on_a_pair_bearer() {
    let _guard = ENV_LOCK.lock().await;
    // The other dispatch path. This op never reaches the plugin
    // router, so a gate that lives only there cannot see it.
    let (booted, _dir) = boot(open_policy()).await;
    let (_id, pair) = mint_pair_bearer(&booted);
    booted
        .server
        .household_protection()
        .expect("runtime attached")
        .apply(strict_policy())
        .expect("policy applies");

    let (status, body) =
        post_as(&booted, &pair, FRAMEWORK_OP, framework_protected_body()).await;
    assert_eq!(
        subclass_of(&body).as_deref(),
        Some("household_policy_locked"),
        "a protected scope is protected on both dispatch paths. \
         status={status} body={body}"
    );
}

#[tokio::test]
async fn a_step_up_session_admits_the_framework_op_without_widening() {
    let _guard = ENV_LOCK.lock().await;
    // The override: the owner proves they are the owner and the
    // write goes through. The level is untouched by that.
    let (booted, _dir) = boot(open_policy()).await;
    let (pair_id, pair) = mint_pair_bearer(&booted);
    let runtime = booted
        .server
        .household_protection()
        .expect("runtime attached");
    runtime.apply(strict_policy()).expect("policy applies");

    let session = booted.auth.issue(
        evo::auth::VerifiedPrincipal {
            username: "operator".to_owned(),
            uid: Some(0),
        },
        evo::auth::caller_identity(Some(0), Some(&pair_id)),
        None,
    );
    let mut body = framework_protected_body();
    body["step_up_token"] = serde_json::json!(session.token);

    let (status, out) = post_as(&booted, &pair, FRAMEWORK_OP, body).await;
    // The claim under test is that the gate does not refuse: the
    // write reaches its handler. This harness wires no device
    // identity store, so the handler then declines on its own —
    // which is itself the evidence that the request got past the
    // gate rather than being stopped at it.
    assert_ne!(
        subclass_of(&out).as_deref(),
        Some("household_policy_locked"),
        "a live step-up session for this caller must admit the write. \
         status={status} out={out}"
    );
    assert_eq!(
        runtime.snapshot().level,
        ProtectionLevel::Strict,
        "the override must not change the level"
    );
}

#[tokio::test]
async fn strict_still_admits_playback_on_a_pair_bearer() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(open_policy()).await;
    let (_id, pair) = mint_pair_bearer(&booted);
    booted
        .server
        .household_protection()
        .expect("runtime attached")
        .apply(strict_policy())
        .expect("policy applies");
    let (status, body) = post_as(&booted, &pair, "request", play_body()).await;
    assert_eq!(status, 200, "playback is never locked. body={body}");
}

#[tokio::test]
async fn the_same_framework_op_without_a_session_is_refused() {
    let _guard = ENV_LOCK.lock().await;
    // Control for the override case above: identical call, no
    // step-up token. If this were admitted too, the override would
    // be proving nothing.
    let (booted, _dir) = boot(open_policy()).await;
    let (_id, pair) = mint_pair_bearer(&booted);
    booted
        .server
        .household_protection()
        .expect("runtime attached")
        .apply(strict_policy())
        .expect("policy applies");
    let (status, out) =
        post_as(&booted, &pair, FRAMEWORK_OP, framework_protected_body()).await;
    assert_eq!(
        subclass_of(&out).as_deref(),
        Some("household_policy_locked"),
        "without a session the same write must be refused. \
         status={status} out={out}"
    );
}

// ---------------------------------------------------------------
// The sitting is a transport credential: taken off the payload
// once, before typed parse, and carried for the dispatch.
//
// Before, each request shape declared its own copy. Most did not,
// and ClientRequest is deny_unknown_fields — so presenting a
// sitting to an op that had not declared it refused the op
// outright, which is what the operator surface met on its first
// call after an override.
// ---------------------------------------------------------------

fn invalid_payload(body: &Value) -> bool {
    let s = body.to_string();
    s.contains("invalid_payload")
        || s.contains("unknown field")
        || s.contains("parse error")
}

async fn with_sitting(
    booted: &Booted,
    bearer_id: &str,
    mut body: Value,
) -> Value {
    let session = booted.auth.issue(
        evo::auth::VerifiedPrincipal {
            username: "operator".to_owned(),
            uid: Some(0),
        },
        evo::auth::caller_identity(Some(0), Some(bearer_id)),
        None,
    );
    body["step_up_token"] = serde_json::json!(session.token);
    body
}

#[tokio::test]
async fn override_admits_metadata_first_paint_credential_list_keys() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(open_policy()).await;
    let (id, pair) = mint_pair_bearer(&booted);
    let body = with_sitting(
        &booted,
        &id,
        serde_json::json!({
            "op": "credential_list_keys",
            "plugin_id": "org.example.plugin",
        }),
    )
    .await;
    let (status, out) =
        post_as(&booted, &pair, "credential_list_keys", body).await;
    assert!(
        !invalid_payload(&out),
        "presenting a sitting must not refuse the op. status={status} out={out}"
    );
}

#[tokio::test]
async fn override_admits_metadata_write_credential_put() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(open_policy()).await;
    let (id, pair) = mint_pair_bearer(&booted);
    let body = with_sitting(
        &booted,
        &id,
        serde_json::json!({
            "op": "credential_put",
            "plugin_id": "org.example.plugin",
            "key": "listenbrainz_token",
            "value_b64": "YQ==",
        }),
    )
    .await;
    let (status, out) = post_as(&booted, &pair, "credential_put", body).await;
    assert!(
        !invalid_payload(&out),
        "credential_put must accept a sitting. status={status} out={out}"
    );
}

#[tokio::test]
async fn override_admits_metadata_write_online_providers_set_enabled() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot(open_policy()).await;
    let (id, pair) = mint_pair_bearer(&booted);
    let body = with_sitting(
        &booted,
        &id,
        serde_json::json!({
            "op": "online_providers_set_enabled",
            "provider_id": "listenbrainz",
            "enabled": false,
        }),
    )
    .await;
    let (status, out) =
        post_as(&booted, &pair, "online_providers_set_enabled", body).await;
    assert!(
        !invalid_payload(&out),
        "online_providers_set_enabled must accept a sitting. \
         status={status} out={out}"
    );
}

#[tokio::test]
async fn override_admits_file_sharing_write_through_request() {
    let _guard = ENV_LOCK.lock().await;
    // The shelf-verb path. `request` DID declare the field before
    // this change, so this is the arm that proves the threaded
    // sitting still reaches handle_plugin_request.
    let (booted, _dir) = boot(open_policy()).await;
    let (id, pair) = mint_pair_bearer(&booted);
    booted
        .server
        .household_protection()
        .expect("runtime attached")
        .apply(strict_policy())
        .expect("policy applies");
    let body = with_sitting(&booted, &id, request_body()).await;
    let (status, out) = post_as(&booted, &pair, "request", body).await;
    assert!(
        !invalid_payload(&out),
        "a shelf verb must still accept a sitting. status={status} out={out}"
    );
    assert_ne!(
        subclass_of(&out).as_deref(),
        Some("household_policy_locked"),
        "a live sitting admits the shelf write. status={status} out={out}"
    );
}

#[tokio::test]
async fn file_sharing_read_get_state_needs_no_sitting() {
    let _guard = ENV_LOCK.lock().await;
    // A verb declaring no capability never reaches the gate and
    // needs no sitting, strict or not.
    let (booted, _dir) = boot(open_policy()).await;
    let (_id, pair) = mint_pair_bearer(&booted);
    booted
        .server
        .household_protection()
        .expect("runtime attached")
        .apply(strict_policy())
        .expect("policy applies");
    let (status, out) = post_as(&booted, &pair, "request", play_body()).await;
    assert_eq!(status, 200, "an ungated verb needs no sitting. out={out}");
}
