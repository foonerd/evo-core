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
//!
//! At a household level that protects the network group, the nest
//! inherits the household sitting of the consumer whose request the
//! relay is answering:
//!
//! - a consumer that presented a genuine sitting gets the nest
//!   admitted, and the token reaches neither plugin;
//! - a consumer with no sitting, or one minted for another browser,
//!   gets the nest refused `household_policy_locked`, and the relay
//!   sees that refusal by name;
//! - two consumers waiting on the relay at once give the nest nothing,
//!   whatever sittings they hold.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::household_protection::{
    HouseholdProtectionPolicy, HouseholdProtectionRuntime, ProtectionLevel,
};
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
use evo_plugin_sdk::Manifest;
use serde_json::{json, Value};
use tokio::sync::{Barrier, Mutex};

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

/// Every payload a plugin received, in order.
type Seen = Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

/// The network writer. Counts what reaches it, so "admitted" is read
/// from evidence rather than from a response shape, and keeps each
/// payload it was handed.
struct Writer {
    reached: Arc<AtomicUsize>,
    seen: Seen,
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
            self.seen.lock().unwrap().push(req.payload.clone());
            Ok(Response::for_request(
                req,
                json!({"set": true}).to_string().into_bytes(),
            ))
        }
    }
}

/// Two barriers the relay meets around its nest: one before it, so
/// two consumers are both open on the relay when either nests, and one
/// after it, so neither leaves before the other's nest has been gated.
#[derive(Clone)]
struct Rendezvous {
    before: Arc<Barrier>,
    after: Arc<Barrier>,
}

/// The relay. Asked for its one verb, it nests a dispatch to the
/// writer through the dispatcher its load context carries, and answers
/// with what the nest came back with. A refusal that names itself is
/// answered by name.
struct Relay {
    dispatcher: Option<Arc<dyn ShelfRequestDispatcher>>,
    seen: Seen,
    rendezvous: Arc<std::sync::Mutex<Option<Rendezvous>>>,
}

/// Wait at `barrier`, or give up and say so rather than hang the test.
async fn meet(barrier: &Barrier) -> Result<(), PluginError> {
    tokio::time::timeout(Duration::from_secs(10), barrier.wait())
        .await
        .map(|_| ())
        .map_err(|_| PluginError::Permanent("rendezvous timed out".into()))
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
            self.seen.lock().unwrap().push(req.payload.clone());
            let dispatcher = self.dispatcher.as_ref().ok_or_else(|| {
                PluginError::Permanent(
                    "no shelf request dispatcher on the load context".into(),
                )
            })?;
            let rendezvous = self.rendezvous.lock().unwrap().clone();
            if let Some(r) = &rendezvous {
                meet(&r.before).await?;
            }
            let nested = match dispatcher
                .dispatch(WRITER_SHELF, WRITE_VERB, b"{}".to_vec(), None)
                .await
            {
                Ok(bytes) => json!({
                    "nest": "admitted",
                    "answer": serde_json::from_slice::<Value>(&bytes)
                        .unwrap_or(Value::Null),
                }),
                Err(ShelfDispatchError::Refused { subclass, detail }) => {
                    json!({
                        "nest": "refused",
                        "subclass": subclass,
                        "error": detail,
                    })
                }
                Err(e) => json!({"nest": "refused", "error": e.to_string()}),
            };
            if let Some(r) = &rendezvous {
                meet(&r.after).await?;
            }
            Ok(Response::for_request(req, nested.to_string().into_bytes()))
        }
    }
}

struct Booted {
    addr: std::net::SocketAddr,
    ca_pem: String,
    reached: Arc<AtomicUsize>,
    writer_seen: Seen,
    relay_seen: Seen,
    relay_rendezvous: Arc<std::sync::Mutex<Option<Rendezvous>>>,
    // Held so the Weak the dispatcher keeps stays upgradable, and so a
    // case can move the household level.
    server: Arc<Server>,
    /// The store the steward validates step-up tokens against, so a
    /// case can mint a genuine sitting.
    auth: Arc<evo::auth::AuthSessionStore>,
    /// For the bearer issuer the HTTPS validator verifies against.
    state: Arc<StewardState>,
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
    let writer_seen = Seen::default();
    let relay_seen = Seen::default();
    let relay_rendezvous = Arc::new(std::sync::Mutex::new(None));
    engine
        .admit_singleton_respondent(
            Writer {
                reached: Arc::clone(&reached),
                seen: Arc::clone(&writer_seen),
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
            Relay {
                dispatcher: None,
                seen: Arc::clone(&relay_seen),
                rendezvous: Arc::clone(&relay_rendezvous),
            },
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
        .with_auth(None, Arc::clone(&auth)),
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
            writer_seen,
            relay_seen,
            relay_rendezvous,
            server,
            auth,
            state,
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
    ask(booted, None, shelf, verb, None).await
}

/// Ask `verb` on `shelf` from loopback, as `bearer` when given, with
/// `step_up_token` on the request when given.
async fn ask(
    booted: &Booted,
    bearer: Option<&str>,
    shelf: &str,
    verb: &str,
    step_up_token: Option<&str>,
) -> (u16, Value) {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let mut body = json!({
        "shelf": shelf,
        "request_type": verb,
        "payload_b64": B64.encode(b"{}"),
    });
    if let Some(token) = step_up_token {
        body["step_up_token"] = json!(token);
    }
    let mut req = client_with_ca(&booted.ca_pem)
        .post(format!("https://{}/api/v1/request", booted.addr))
        .json(&body);
    if let Some(bearer) = bearer {
        req = req.bearer_auth(bearer);
    }
    let resp = req.send().await.unwrap();
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

// ---------------------------------------------------------------
// The household sitting the nest inherits.
//
// At Strict the household protects the network group, so the nest's
// write is admitted only on a sitting. The nest carries none of its
// own; it inherits the sitting of the consumer whose request the
// relay is answering, or it is refused.
// ---------------------------------------------------------------

fn strict() -> HouseholdProtectionPolicy {
    HouseholdProtectionPolicy {
        chosen: true,
        level: ProtectionLevel::Strict,
        ..Default::default()
    }
}

/// Boot under `lan_privilege` and move the household to Strict.
async fn boot_strict(
    lan_privilege: LanPrivilege,
) -> (Booted, tempfile::TempDir) {
    let (booted, dir) = boot(lan_privilege).await;
    booted
        .server
        .household_protection()
        .expect("household runtime attached")
        .apply(strict())
        .expect("policy applies");
    (booted, dir)
}

/// A paired browser: operator capabilities, its own bearer. Returns
/// the bearer's id and its encoded form.
fn pair(booted: &Booted) -> (String, String) {
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

/// A genuine sitting, in the store the steward validates against,
/// bound to the browser whose bearer id is `bearer_id`.
fn sitting_for(booted: &Booted, bearer_id: &str) -> String {
    booted
        .auth
        .issue(
            evo::auth::VerifiedPrincipal {
                username: "operator".to_owned(),
                uid: Some(0),
            },
            evo::auth::caller_identity(Some(0), Some(bearer_id)),
            None,
        )
        .token
}

/// The relay answered, and its nest was refused by the household,
/// by name, without reaching the writer.
fn assert_nest_locked(
    booted: &Booted,
    status: u16,
    answer: &Value,
    case: &str,
) {
    assert_eq!(status, 200, "{case}: the relay itself answers. {answer}");
    assert_eq!(answer["nest"], "refused", "{case}: {answer}");
    assert_eq!(
        answer["subclass"], "household_policy_locked",
        "{case}: the relay must see the household refusal by name. {answer}"
    );
    assert_eq!(
        booted.reached.load(Ordering::SeqCst),
        0,
        "{case}: the writer must not be reached"
    );
}

#[tokio::test]
async fn strict_admits_a_nest_whose_consumer_holds_a_sitting() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot_strict(LanPrivilege::PolicedByHousehold).await;
    let (id, bearer) = pair(&booted);
    let token = sitting_for(&booted, &id);

    let (status, answer) = ask(
        &booted,
        Some(&bearer),
        RELAY_SHELF,
        RELAY_VERB,
        Some(&token),
    )
    .await;
    assert_eq!(status, 200, "answer={answer}");
    assert_eq!(
        answer["nest"], "admitted",
        "the nest must inherit the consumer's sitting. answer={answer}"
    );
    assert_eq!(answer["answer"], json!({"set": true}), "answer={answer}");
    assert_eq!(booted.reached.load(Ordering::SeqCst), 1);

    // The sitting travelled beside the plugins, never through them.
    for (plugin, seen) in [
        ("relay", &booted.relay_seen),
        ("writer", &booted.writer_seen),
    ] {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{plugin} handled one request");
        let payload = String::from_utf8_lossy(&seen[0]);
        assert!(
            !payload.contains(&token) && !payload.contains("step_up_token"),
            "the token must never reach the {plugin}'s payload: {payload}"
        );
    }

    // Asking the writer directly with the same sitting is admitted too:
    // the nest gets what the consumer would get, no more.
    let (status, direct) = ask(
        &booted,
        Some(&bearer),
        WRITER_SHELF,
        WRITE_VERB,
        Some(&token),
    )
    .await;
    assert_eq!(status, 200, "body={direct}");
}

#[tokio::test]
async fn strict_refuses_a_nest_whose_consumer_holds_no_sitting() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot_strict(LanPrivilege::PolicedByHousehold).await;
    let (_id, bearer) = pair(&booted);

    let (status, answer) =
        ask(&booted, Some(&bearer), RELAY_SHELF, RELAY_VERB, None).await;
    assert_nest_locked(&booted, status, &answer, "paired, no sitting");
}

#[tokio::test]
async fn strict_refuses_a_nest_for_an_anonymous_consumer_under_either_lan_choice(
) {
    let _guard = ENV_LOCK.lock().await;
    for lan_privilege in
        [LanPrivilege::PolicedByHousehold, LanPrivilege::Withheld]
    {
        let (booted, _dir) = boot_strict(lan_privilege).await;
        let (status, answer) =
            ask_anonymously(&booted, RELAY_SHELF, RELAY_VERB).await;
        assert_nest_locked(
            &booted,
            status,
            &answer,
            &format!("anonymous, {lan_privilege:?}"),
        );
    }
}

#[tokio::test]
async fn strict_refuses_a_nest_on_a_sitting_minted_for_another_browser() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot_strict(LanPrivilege::PolicedByHousehold).await;
    let (other_id, _other) = pair(&booted);
    let (_id, bearer) = pair(&booted);
    // Genuine, but bound to the other browser. The relay is ungated, so
    // the outer call passes; the nest validates the token against the
    // consumer that presented it and finds it is not theirs.
    let token = sitting_for(&booted, &other_id);

    let (status, answer) = ask(
        &booted,
        Some(&bearer),
        RELAY_SHELF,
        RELAY_VERB,
        Some(&token),
    )
    .await;
    assert_nest_locked(&booted, status, &answer, "another browser's sitting");
}

#[tokio::test]
async fn strict_refuses_a_nest_while_two_consumers_wait_on_the_relay() {
    let _guard = ENV_LOCK.lock().await;
    let (booted, _dir) = boot_strict(LanPrivilege::PolicedByHousehold).await;
    let (a_id, a) = pair(&booted);
    let (b_id, b) = pair(&booted);
    let a_token = sitting_for(&booted, &a_id);
    let b_token = sitting_for(&booted, &b_id);

    // Both consumers hold a genuine sitting. While both wait on the
    // relay, a nest from it cannot be attributed to either.
    *booted.relay_rendezvous.lock().unwrap() = Some(Rendezvous {
        before: Arc::new(Barrier::new(2)),
        after: Arc::new(Barrier::new(2)),
    });
    let ((a_status, a_answer), (b_status, b_answer)) = tokio::join!(
        ask(&booted, Some(&a), RELAY_SHELF, RELAY_VERB, Some(&a_token)),
        ask(&booted, Some(&b), RELAY_SHELF, RELAY_VERB, Some(&b_token)),
    );
    assert_nest_locked(&booted, a_status, &a_answer, "consumer A of two");
    assert_nest_locked(&booted, b_status, &b_answer, "consumer B of two");

    // Alone again, the same consumer's nest is admitted: the records
    // left with the dispatches that made them.
    *booted.relay_rendezvous.lock().unwrap() = None;
    let (status, answer) =
        ask(&booted, Some(&a), RELAY_SHELF, RELAY_VERB, Some(&a_token)).await;
    assert_eq!(status, 200, "answer={answer}");
    assert_eq!(answer["nest"], "admitted", "answer={answer}");
    assert_eq!(booted.reached.load(Ordering::SeqCst), 1);
}
