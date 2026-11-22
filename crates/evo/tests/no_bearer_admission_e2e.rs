// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! What a caller without a bearer can reach, over real wires.
//!
//! A caller admitted without a bearer holds a broad-audience set: the
//! floor, or on the LAN of a home player the privileged set. Nothing
//! in either may turn into a bearer, and no network caller may borrow
//! the steward's own peer credentials. These pins hold that:
//!
//! - minting (`create_bearer_token`) needs a bearer of one's own, and
//!   grants only what that bearer holds;
//! - the operator credential inventory (`list_bearer_tokens`,
//!   `revoke_bearer_token`) and the OS-password set
//!   (`set_kiosk_password`) need a bearer of one's own;
//! - the kiosk mint is served on the local socket only;
//! - pairing is served to the LAN only, on `write:pair`, which every
//!   no-bearer set carries and no distribution bound removes;
//! - a distribution may bound the no-bearer sets down to pairing;
//! - an admission by peer uid never admits a network caller;
//! - the local-socket operator still mints;
//! - the signing key rotates offline, through `evo::run`.
//!
//! Everything goes through a public path: HTTPS (REST and the
//! operator WebSocket) through the listener `maybe_boot_https`
//! mounts, the steward's Unix socket, and `evo::run` for the
//! subcommands. Principals are stamped by the middleware or minted by
//! the steward's own issuer; none is fabricated.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::client_acl::{ClientAcl, StewardIdentity};
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::household_protection::HouseholdProtectionRuntime;
use evo::kiosk_session::KioskUidAllowlist;
use evo::persistence::MemoryPersistenceStore;
use evo::projections::ProjectionEngine;
use evo::relations::RelationGraph;
use evo::resolution::ResolutionLedger;
use evo::server::Server;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo::{maybe_boot_https, LanPrivilege};
use evo_auth_bearer::{
    BearerToken, BearerTokenIssuer, Capability, CapabilitySet,
    DEFAULT_TOKEN_TTL_MS,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

/// The boot reads its auth tier and state directory from the
/// environment, which is process-wide.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "panel"
family = "domain"
charter = "One-shelf rack for the no-bearer admission pins."

[[racks.shelves]]
name = "display"
shape = 1
description = "An empty shelf; these pins dispatch framework ops only."
"#;

/// The preseeded headless pairing code (`pair_id: "bootstrap"`), the
/// path a headless device's face pairs on.
const PRESEED: &str = "13572468";
/// An address no LAN classification admits (TEST-NET-3).
const PUBLIC: &str = "203.0.113.7";

/// The steward's identity when it runs as root: what makes a
/// synthetic uid 0 dangerous if any admission ever trusted it.
const ROOT: StewardIdentity = StewardIdentity {
    uid: Some(0),
    gid: Some(0),
};

struct Options {
    tier: Option<&'static str>,
    lan_privilege: LanPrivilege,
    bound: Option<CapabilitySet>,
    identity: StewardIdentity,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            tier: None,
            lan_privilege: LanPrivilege::default(),
            bound: None,
            identity: StewardIdentity::current(),
        }
    }
}

struct Booted {
    addr: std::net::SocketAddr,
    ca_pem: String,
    state: Arc<StewardState>,
    issuer: Arc<BearerTokenIssuer>,
    socket_path: PathBuf,
    _handles: evo::https_boot::HttpsBootHandles,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
}

/// Boot a steward with HTTPS and its Unix socket. The kiosk allowlist
/// admits uid 0, so a network caller that borrowed the synthetic uid
/// would be admitted: the pins below prove none does.
async fn boot(options: Options) -> Booted {
    let dir = tempfile::tempdir().expect("state tempdir");
    let parent = dir.path().to_path_buf();
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
            "no-bearer-admission",
        )))
        .build()
        .expect("steward state");
    let engine = AdmissionEngine::new(
        Arc::clone(&state),
        parent.join("data-root"),
        PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    );
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
            options.identity,
            Arc::new(ResolutionLedger::new()),
            evo::catalogue::CatalogueSource::Configured,
            evo::time_trust::new_shared(),
            evo::time_trust::new_shared_last_step(),
            false,
        )
        .with_household_protection(household)
        .with_auth(None, Arc::new(evo::auth::AuthSessionStore::with_defaults()))
        .with_kiosk_uid_allowlist(Arc::new(KioskUidAllowlist::from_uids([0])))
        .with_bootstrap_pair_preseed(PRESEED.to_string(), "test".to_string()),
    );

    let handles = {
        let _env = ENV_LOCK.lock().await;
        std::env::set_var("EVO_HTTPS_LISTEN_ADDR", "127.0.0.1:0");
        std::env::set_var("EVO_HTTPS_STATE_DIR", &parent);
        match options.tier {
            Some(tier) => std::env::set_var("EVO_AUTH_TIER", tier),
            None => std::env::remove_var("EVO_AUTH_TIER"),
        }
        let handles = maybe_boot_https(
            Arc::clone(&server),
            &parent.join("persistence.sqlite"),
            None,
            None,
            options.lan_privilege,
            options.bound,
        )
        .await
        .expect("maybe_boot_https Ok")
        .expect("HTTPS listener mounted");
        std::env::remove_var("EVO_AUTH_TIER");
        handles
    };
    // What `run()` does after the HTTPS boot: the mint ops find the
    // issuer and the inventory on steward state.
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
    wait_for_socket(&socket_path).await;

    let ca_pem =
        std::fs::read_to_string(parent.join("https").join("ca.crt")).unwrap();
    Booted {
        addr: handles.server.local_addr,
        ca_pem,
        state,
        issuer: Arc::clone(&handles.issuer),
        socket_path,
        _handles: handles,
        _shutdown: shutdown,
        _dir: dir,
    }
}

async fn wait_for_socket(path: &Path) {
    for _ in 0..200 {
        if UnixStream::connect(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("steward socket {} never answered", path.display());
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

impl Booted {
    /// A bearer from the steward's own issuer, carrying `caps`.
    fn bearer(&self, caps: Vec<Capability>) -> String {
        self.issuer
            .issue(CapabilitySet::new(caps), DEFAULT_TOKEN_TTL_MS, now_ms())
            .expect("issue bearer")
            .encode()
    }

    /// The operator credential inventory, as the steward keeps it.
    fn records(&self) -> usize {
        self.state
            .credential_store
            .get()
            .expect("credential store")
            .list()
            .expect("list records")
            .len()
    }

    /// One REST call. `bearer` goes in `Authorization`; `from` is the
    /// caller's address as a trusted loopback proxy forwards it.
    async fn rest(
        &self,
        method: reqwest::Method,
        op: &str,
        body: Value,
        bearer: Option<&str>,
        from: Option<&str>,
    ) -> (u16, Value) {
        let cert =
            reqwest::Certificate::from_pem(self.ca_pem.as_bytes()).unwrap();
        let client = reqwest::Client::builder()
            .add_root_certificate(cert)
            .tls_built_in_root_certs(false)
            .danger_accept_invalid_hostnames(true)
            .pool_max_idle_per_host(0)
            .build()
            .unwrap();
        let mut req = client.request(
            method.clone(),
            format!("https://{}/api/v1/{op}", self.addr),
        );
        if method != reqwest::Method::GET {
            req = req.json(&body);
        }
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        if let Some(ip) = from {
            req = req.header("x-forwarded-for", ip);
        }
        let resp = req.send().await.expect("REST send");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn post(
        &self,
        op: &str,
        body: Value,
        bearer: Option<&str>,
        from: Option<&str>,
    ) -> (u16, Value) {
        self.rest(reqwest::Method::POST, op, body, bearer, from)
            .await
    }

    /// Open the operator WebSocket. `Err` carries the refusal.
    async fn ws(
        &self,
        bearer: Option<&str>,
        from: Option<&str>,
    ) -> Result<Ws, String> {
        evo_runtime_http::install_crypto_provider();
        let mut req = format!("wss://{}/api/v1/ws", self.addr)
            .into_client_request()
            .unwrap();
        if let Some(b) = bearer {
            req.headers_mut().insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {b}")).unwrap(),
            );
        }
        if let Some(ip) = from {
            req.headers_mut()
                .insert("x-forwarded-for", HeaderValue::from_str(ip).unwrap());
        }
        let mut roots = rustls::RootCertStore::empty();
        let mut reader = std::io::Cursor::new(self.ca_pem.as_bytes());
        for cert in rustls_pemfile::certs(&mut reader) {
            roots.add(cert.unwrap()).unwrap();
        }
        let cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        connect_async_tls_with_config(
            req,
            None,
            false,
            Some(Connector::Rustls(Arc::new(cfg))),
        )
        .await
        .map(|(stream, _)| Ws { stream, next_id: 1 })
        .map_err(|e| format!("{e:?}"))
    }
}

struct Ws {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl Ws {
    /// Send one request frame; return its response `outcome`.
    async fn ask(&mut self, op: &str, payload: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({
            "frame_type": "request",
            "request_id": id,
            "op": op,
            "payload": payload,
        });
        self.stream
            .send(Message::Text(frame.to_string()))
            .await
            .expect("WS send");
        loop {
            let msg = tokio::time::timeout(
                Duration::from_secs(10),
                self.stream.next(),
            )
            .await
            .expect("WS answer in time")
            .expect("WS open")
            .expect("WS frame");
            let Message::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(&text).expect("WS JSON");
            if v["response_to"] == json!(id) {
                return v["outcome"].clone();
            }
        }
    }
}

/// The refusal subclass of a REST body or a WS outcome.
fn subclass(v: &Value) -> Option<&str> {
    v.pointer("/error/details/subclass")
        .or_else(|| v.pointer("/subclass"))
        .and_then(Value::as_str)
}

fn mint_body(scopes: Value) -> Value {
    json!({ "name": "pin", "reason": "admission pin", "scopes": scopes })
}

fn caps_of(encoded: &str) -> CapabilitySet {
    BearerToken::decode(encoded)
        .expect("minted token decodes")
        .capabilities
}

// ---------------------------------------------------------------------
// Minting
// ---------------------------------------------------------------------

/// No tier and no origin lets a caller without a bearer mint one: not
/// over REST, not over the WebSocket, not from the LAN, not from a
/// public address under `open`. Secure tiers refuse a public origin at
/// admission already.
#[tokio::test]
async fn a_caller_with_no_bearer_cannot_mint() {
    for tier in [None, Some("secure")] {
        let b = boot(Options {
            tier,
            ..Options::default()
        })
        .await;
        let records = b.records();
        for from in [None, Some(PUBLIC)] {
            let admitted = tier.is_none() || from.is_none();
            let (status, body) = b
                .post("create_bearer_token", mint_body(json!([])), None, from)
                .await;
            if admitted {
                assert_eq!(status, 403, "tier {tier:?} from {from:?}: {body}");
                assert_eq!(subclass(&body), Some("bearer_required"), "{body}");
            } else {
                assert_eq!(status, 401, "tier {tier:?} from {from:?}");
            }
            match b.ws(None, from).await {
                Ok(mut ws) => {
                    assert!(admitted, "tier {tier:?} from {from:?} upgraded");
                    let out = ws
                        .ask("create_bearer_token", mint_body(json!([])))
                        .await;
                    assert_eq!(out["outcome"], "err", "{out}");
                    assert_eq!(
                        subclass(&out),
                        Some("bearer_required"),
                        "{out}"
                    );
                }
                Err(e) => {
                    assert!(!admitted, "tier {tier:?} from {from:?}: {e}");
                    assert!(e.contains("401"), "{e}");
                }
            }
        }
        assert_eq!(b.records(), records, "nothing was minted");
    }
}

/// A bearer grants only what it holds: no scope it lacks, no rank
/// above the one it holds. Lower ranks of what it holds are fine.
#[tokio::test]
async fn a_caller_cannot_mint_a_scope_it_does_not_hold() {
    let b = boot(Options::default()).await;
    let bearer = b.bearer(vec![
        Capability::write("auth"),
        Capability::write("plugins"),
        Capability::read("audio"),
    ]);
    for (asked, why) in [
        (
            json!([{"kind": "step_up", "scope": "plugins"}]),
            "rank above held",
        ),
        (
            json!([{"kind": "write", "scope": "system_admin"}]),
            "scope not held",
        ),
        (
            json!([{"kind": "write", "scope": "audio"}]),
            "read held, write asked",
        ),
    ] {
        let (status, body) = b
            .post("create_bearer_token", mint_body(asked), Some(&bearer), None)
            .await;
        assert_eq!(status, 403, "{why}: {body}");
        assert_eq!(subclass(&body), Some("scope_not_held"), "{why}: {body}");
    }
    let asked = json!([
        {"kind": "read", "scope": "plugins"},
        {"kind": "read", "scope": "audio"},
    ]);
    let (status, body) = b
        .post("create_bearer_token", mint_body(asked), Some(&bearer), None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        caps_of(body["token"].as_str().unwrap()),
        CapabilitySet::new(vec![
            Capability::read("plugins"),
            Capability::read("audio")
        ]),
    );

    let mut ws = b.ws(Some(&bearer), None).await.expect("bearer upgrade");
    let out = ws
        .ask(
            "create_bearer_token",
            mint_body(json!([{"kind": "step_up", "scope": "plugins_admin"}])),
        )
        .await;
    assert_eq!(subclass(&out), Some("scope_not_held"), "{out}");
}

/// Empty scopes mint the caller's own set, never the operator
/// bootstrap set.
#[tokio::test]
async fn empty_scopes_mint_only_the_callers_own_set() {
    let b = boot(Options::default()).await;
    let held = CapabilitySet::new(vec![
        Capability::write("auth"),
        Capability::read("audio"),
    ]);
    let bearer = b.bearer(held.capabilities().to_vec());
    let (status, body) = b
        .post(
            "create_bearer_token",
            mint_body(json!([])),
            Some(&bearer),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(caps_of(body["token"].as_str().unwrap()), held);
    assert_eq!(body["record"]["scopes"].as_array().unwrap().len(), 2);
}

/// The local-socket operator still mints, unchanged: negotiated
/// `plugins_admin`, and empty scopes give every operator scope.
#[tokio::test]
async fn the_socket_operator_still_mints() {
    let b = boot(Options::default()).await;
    let mut stream = UnixStream::connect(&b.socket_path).await.unwrap();
    let granted = unix_ask(
        &mut stream,
        json!({"op": "negotiate", "capabilities": ["plugins_admin"]}),
    )
    .await;
    assert_eq!(granted["granted"], json!(["plugins_admin"]), "{granted}");
    let minted = unix_ask(
        &mut stream,
        json!({"op": "create_bearer_token", "name": "cli", "reason": "pin", "scopes": []}),
    )
    .await;
    assert_eq!(
        caps_of(
            minted["token"]
                .as_str()
                .unwrap_or_else(|| panic!("{minted}"))
        ),
        evo::https_boot::operator_bootstrap_capability_set(),
    );
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

// ---------------------------------------------------------------------
// The credential inventory and the OS password
// ---------------------------------------------------------------------

/// Listing and revoking the operator's credentials needs a bearer of
/// one's own, holding the scope by rank.
#[tokio::test]
async fn a_caller_with_no_bearer_cannot_list_or_revoke_credentials() {
    let b = boot(Options::default()).await;
    let records = b.records();
    let (status, body) = b
        .rest(
            reqwest::Method::GET,
            "list_bearer_tokens",
            Value::Null,
            None,
            None,
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(subclass(&body), Some("bearer_required"), "{body}");

    let operator = b.bearer(
        evo::https_boot::operator_bootstrap_capability_set()
            .capabilities()
            .to_vec(),
    );
    let (_, minted) = b
        .post(
            "create_bearer_token",
            mint_body(json!([])),
            Some(&operator),
            None,
        )
        .await;
    let token_id = minted["record"]["token_id"].as_str().unwrap().to_string();
    let revoke = json!({ "token_id": token_id, "reason": "pin" });

    let (status, body) = b
        .rest(
            reqwest::Method::DELETE,
            "revoke_bearer_token",
            revoke.clone(),
            None,
            None,
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(subclass(&body), Some("bearer_required"), "{body}");
    let mut ws = b.ws(None, None).await.expect("no-bearer LAN upgrade");
    let out = ws.ask("revoke_bearer_token", revoke.clone()).await;
    assert_eq!(subclass(&out), Some("bearer_required"), "{out}");
    let out = ws.ask("list_bearer_tokens", json!({})).await;
    assert_eq!(subclass(&out), Some("bearer_required"), "{out}");

    // By rank: read lists, and only write revokes.
    let reader = b.bearer(vec![Capability::read("auth")]);
    let (status, _) = b
        .rest(
            reqwest::Method::GET,
            "list_bearer_tokens",
            Value::Null,
            Some(&reader),
            None,
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = b
        .rest(
            reqwest::Method::DELETE,
            "revoke_bearer_token",
            revoke.clone(),
            Some(&reader),
            None,
        )
        .await;
    assert_eq!(status, 403, "the route needs write:auth");
    let (status, body) = b
        .rest(
            reqwest::Method::DELETE,
            "revoke_bearer_token",
            revoke,
            Some(&operator),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(b.records(), records + 1);
}

/// Setting the OS password over the network needs a bearer of one's
/// own holding `plugins_admin`. No-bearer callers and short bearers
/// are refused before anything runs.
#[tokio::test]
async fn a_caller_with_no_bearer_cannot_set_the_kiosk_password() {
    let b = boot(Options::default()).await;
    let body = json!({ "new_password": "correct horse battery" });
    for from in [None, Some(PUBLIC)] {
        let (status, out) = b
            .rest(
                reqwest::Method::PUT,
                "set_kiosk_password",
                body.clone(),
                None,
                from,
            )
            .await;
        assert_eq!(status, 403, "{out}");
        assert_eq!(subclass(&out), Some("bearer_required"), "{out}");
    }
    let short = b.bearer(vec![
        Capability::write("auth"),
        Capability::read("plugins_admin"),
    ]);
    let (status, out) = b
        .rest(
            reqwest::Method::PUT,
            "set_kiosk_password",
            body,
            Some(&short),
            None,
        )
        .await;
    assert_eq!(status, 403, "{out}");
    assert_eq!(subclass(&out), Some("password_set_not_admitted"), "{out}");
}

// ---------------------------------------------------------------------
// Peer credentials and the kiosk mint
// ---------------------------------------------------------------------

/// On a steward that runs as root, the synthetic uid 0 the network
/// dispatch carries as a binding key matches both the steward's uid
/// and a kiosk allowlist entry. Neither admits a network caller.
#[tokio::test]
async fn a_network_caller_passes_no_peer_uid_admission() {
    let b = boot(Options {
        identity: ROOT,
        ..Options::default()
    })
    .await;
    let mut ws = b.ws(None, None).await.expect("no-bearer LAN upgrade");
    let out = ws
        .ask(
            "negotiate",
            json!({"capabilities": ["plugins_admin", "resolve_claimants", "plans_admin"]}),
        )
        .await;
    assert_eq!(out["outcome"], "ok", "{out}");
    assert_eq!(out["value"]["granted"], json!([]), "{out}");

    // A bearer that is not enough on its own is not topped up by the
    // kiosk allowlist either.
    let short = b.bearer(vec![Capability::write("auth")]);
    let (status, out) = b
        .rest(
            reqwest::Method::PUT,
            "set_kiosk_password",
            json!({ "new_password": "correct horse battery" }),
            Some(&short),
            None,
        )
        .await;
    assert_eq!(status, 403, "{out}");
    assert_eq!(subclass(&out), Some("password_set_not_admitted"), "{out}");
}

/// The kiosk mint is served on the local socket only: refused over
/// REST and the WebSocket, even to a full operator bearer on the LAN
/// of a steward whose allowlist admits uid 0.
#[tokio::test]
async fn the_kiosk_mint_is_not_reachable_over_https() {
    let b = boot(Options {
        identity: ROOT,
        ..Options::default()
    })
    .await;
    let operator = b.bearer(
        evo::https_boot::operator_bootstrap_capability_set()
            .capabilities()
            .to_vec(),
    );
    let ask = json!({ "reason": "kiosk-boot" });
    for bearer in [None, Some(operator.as_str())] {
        let (status, body) = b
            .post("mint_local_kiosk_session", ask.clone(), bearer, None)
            .await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(subclass(&body), Some("local_socket_only"), "{body}");
        let mut ws = b.ws(bearer, None).await.expect("LAN upgrade");
        let out = ws.ask("mint_local_kiosk_session", ask.clone()).await;
        assert_eq!(subclass(&out), Some("local_socket_only"), "{out}");
    }
}

// ---------------------------------------------------------------------
// Pairing and the floor
// ---------------------------------------------------------------------

/// Under `open`, a public origin mints nothing by any route: no
/// bearer, no pairing, no kiosk session.
#[tokio::test]
async fn open_tier_public_origin_mints_nothing() {
    let b = boot(Options::default()).await;
    let records = b.records();
    let operator = b.bearer(
        evo::https_boot::operator_bootstrap_capability_set()
            .capabilities()
            .to_vec(),
    );
    let asks = [
        (
            "create_bearer_token",
            mint_body(json!([])),
            "bearer_required",
        ),
        (
            "pair_begin",
            json!({"device_hint": "pin"}),
            "pairing_lan_only",
        ),
        (
            "pair_complete",
            json!({"pair_id": "bootstrap", "code": PRESEED}),
            "pairing_lan_only",
        ),
        (
            "pair_authenticate",
            json!({"device_hint": "pin", "secret_b64": "cGFzc3dvcmQ="}),
            "pairing_lan_only",
        ),
        (
            "mint_local_kiosk_session",
            json!({"reason": "kiosk-boot"}),
            "local_socket_only",
        ),
    ];
    for (op, payload, refused) in &asks {
        let (status, body) =
            b.post(op, payload.clone(), None, Some(PUBLIC)).await;
        assert_eq!(status, 403, "{op}: {body}");
        assert_eq!(subclass(&body), Some(*refused), "{op}: {body}");
    }
    let mut ws = b.ws(None, Some(PUBLIC)).await.expect("open admits WAN");
    for (op, payload, refused) in &asks {
        let out = ws.ask(op, payload.clone()).await;
        assert_eq!(subclass(&out), Some(*refused), "{op}: {out}");
    }
    // A bearer does not carry pairing to a public origin either.
    let (status, body) = b
        .post(
            "pair_begin",
            json!({"device_hint": "pin"}),
            Some(&operator),
            Some(PUBLIC),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(subclass(&body), Some("pairing_lan_only"), "{body}");
    assert_eq!(b.records(), records, "no credential record was written");
    // The preseed was never consumed: it still pairs from the LAN.
    let (status, body) = b
        .post(
            "pair_complete",
            json!({"pair_id": "bootstrap", "code": PRESEED}),
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
}

/// A distribution that bounds the no-bearer sets to nothing leaves a
/// caller without a bearer holding pairing and nothing else, under
/// either LAN privilege, on REST and the WebSocket. A headless face
/// pairs exactly so: a REST POST of `pair_complete`.
#[tokio::test]
async fn an_empty_floor_holds_nothing_but_pairing() {
    for lan_privilege in
        [LanPrivilege::Withheld, LanPrivilege::PolicedByHousehold]
    {
        let b = boot(Options {
            lan_privilege,
            bound: Some(CapabilitySet::default()),
            ..Options::default()
        })
        .await;
        let (status, _) = b
            .rest(
                reqwest::Method::GET,
                "household_protection_get",
                Value::Null,
                None,
                None,
            )
            .await;
        assert_eq!(status, 403, "{lan_privilege:?}: read:auth is gone");
        let (status, _) = b
            .post("create_bearer_token", mint_body(json!([])), None, None)
            .await;
        assert_eq!(status, 403, "{lan_privilege:?}: write:auth is gone");
        let (status, _) = b
            .rest(
                reqwest::Method::GET,
                "describe_capabilities",
                Value::Null,
                None,
                None,
            )
            .await;
        assert_eq!(status, 200, "{lan_privilege:?}: discovery stays");

        let mut ws =
            b.ws(None, None).await.expect("no-bearer upgrade still 101");
        let out = ws.ask("household_protection_get", json!({})).await;
        assert_eq!(out["outcome"], "err", "{lan_privilege:?}: {out}");
        let out = ws.ask("pair_begin", json!({"device_hint": "glass"})).await;
        assert_eq!(out["outcome"], "ok", "{lan_privilege:?}: {out}");

        let (status, body) = b
            .post(
                "pair_complete",
                json!({"pair_id": "bootstrap", "code": PRESEED}),
                None,
                None,
            )
            .await;
        assert_eq!(status, 200, "{lan_privilege:?}: {body}");
        let paired = body["token"].as_str().expect("paired bearer").to_string();
        let (status, _) = b
            .rest(
                reqwest::Method::GET,
                "household_protection_get",
                Value::Null,
                Some(&paired),
                None,
            )
            .await;
        assert_eq!(status, 200, "{lan_privilege:?}: the paired bearer reads");
    }
}

/// With no bound the home player keeps its LAN: a browser without a
/// bearer reads, and pairs on the WebSocket (the glass) and over REST.
#[tokio::test]
async fn the_default_floor_keeps_the_home_player() {
    let b = boot(Options::default()).await;
    let (status, body) = b
        .rest(
            reqwest::Method::GET,
            "household_protection_get",
            Value::Null,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let mut ws = b.ws(None, None).await.expect("no-bearer LAN upgrade");
    let out = ws.ask("pair_begin", json!({"device_hint": "glass"})).await;
    assert_eq!(out["outcome"], "ok", "{out}");
    let (status, body) = b
        .post("pair_begin", json!({"device_hint": "browser"}), None, None)
        .await;
    assert_eq!(status, 200, "{body}");
}

/// The cost of `write:pair`: a bearer minted before it existed does
/// not carry it, so pairing on that bearer's own socket is refused.
/// The same browser pairs on a socket without a bearer, which is how
/// the glass pairs.
#[tokio::test]
async fn a_bearer_without_pair_pairs_only_without_its_bearer() {
    let b = boot(Options::default()).await;
    let old =
        b.bearer(vec![Capability::write("auth"), Capability::read("audio")]);
    let (status, _) = b
        .post(
            "pair_begin",
            json!({"device_hint": "old"}),
            Some(&old),
            None,
        )
        .await;
    assert_eq!(status, 403);
    let mut ws = b.ws(Some(&old), None).await.expect("bearer upgrade");
    let out = ws.ask("pair_begin", json!({"device_hint": "old"})).await;
    assert_eq!(out["code"], "permission_denied", "{out}");
    let mut ws = b.ws(None, None).await.expect("no-bearer upgrade");
    let out = ws.ask("pair_begin", json!({"device_hint": "old"})).await;
    assert_eq!(out["outcome"], "ok", "{out}");
}

// ---------------------------------------------------------------------
// Operator subcommands
// ---------------------------------------------------------------------

fn args(argv: &[&str]) -> evo::cli::Args {
    use clap::Parser as _;
    evo::cli::Args::try_parse_from(argv).expect("args parse")
}

/// `auth rotate-token-key` runs through `evo::run` without booting a
/// steward: a fresh key, the inventory and revocations cleared, and a
/// bearer signed before the rotation no longer verifies.
#[tokio::test]
async fn the_token_key_rotates_offline_through_run() {
    let dir = tempfile::tempdir().unwrap();
    let https = dir.path().join("https");
    std::fs::create_dir_all(https.join("credentials")).unwrap();
    let old_key = BearerTokenIssuer::generate_signing_key();
    std::fs::write(https.join("tokens.key"), old_key.to_bytes()).unwrap();
    std::fs::write(https.join("credentials").join("t1.json"), "{}").unwrap();
    std::fs::write(https.join("revoked.json"), "[]").unwrap();
    std::fs::write(https.join("bootstrap.token"), "old").unwrap();
    let config = dir.path().join("evo.toml");
    std::fs::write(&config, "").unwrap();
    let socket = dir.path().join("evo.sock");
    let before = BearerTokenIssuer::new(old_key.clone())
        .issue(CapabilitySet::default(), DEFAULT_TOKEN_TTL_MS, now_ms())
        .unwrap();

    evo::run(evo::RunOptions::from_args(args(&[
        "evo",
        "--config",
        config.to_str().unwrap(),
        "--socket",
        socket.to_str().unwrap(),
        "auth",
        "rotate-token-key",
        "--state-dir",
        dir.path().to_str().unwrap(),
    ])))
    .await
    .expect("rotation");

    assert!(!socket.exists(), "no steward was booted");
    let raw = std::fs::read(https.join("tokens.key")).unwrap();
    assert_eq!(raw.len(), 32);
    assert_ne!(raw.as_slice(), old_key.to_bytes().as_slice());
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(https.join("tokens.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(!https.join("credentials").join("t1.json").exists());
    assert!(!https.join("revoked.json").exists());
    assert!(!https.join("bootstrap.token").exists());

    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&raw);
    let new_key = ed25519_dalek::SigningKey::from_bytes(&bytes);
    let validator = evo_auth_bearer::BearerTokenValidator::new(
        new_key.verifying_key(),
        Arc::new(evo_auth_bearer::RevocationList::new()),
    );
    assert!(validator.verify(&before, now_ms()).is_err());
}

/// The rotation refuses while the steward answers on its socket, and
/// leaves the key as it was.
#[tokio::test]
async fn the_token_key_does_not_rotate_under_a_running_steward() {
    let b = boot(Options::default()).await;
    let state_dir = b.socket_path.parent().unwrap().to_path_buf();
    let key =
        std::fs::read(state_dir.join("https").join("tokens.key")).unwrap();
    let config = state_dir.join("evo.toml");
    std::fs::write(&config, "").unwrap();
    let err = evo::run(evo::RunOptions::from_args(args(&[
        "evo",
        "--config",
        config.to_str().unwrap(),
        "--socket",
        b.socket_path.to_str().unwrap(),
        "auth",
        "rotate-token-key",
        "--state-dir",
        state_dir.to_str().unwrap(),
    ])))
    .await
    .expect_err("refused while running");
    assert!(err.to_string().contains("running"), "{err}");
    assert_eq!(
        std::fs::read(state_dir.join("https").join("tokens.key")).unwrap(),
        key
    );
}

/// `audit` is answered by `run` too, and boots nothing.
#[tokio::test]
async fn audit_does_not_boot_a_steward() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("evo.sock");
    let missing = dir.path().join("absent.db");
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        evo::run(evo::RunOptions::from_args(args(&[
            "evo",
            "--socket",
            socket.to_str().unwrap(),
            "audit",
            "lifecycle",
            "--db",
            missing.to_str().unwrap(),
        ]))),
    )
    .await
    .expect("audit returns rather than serving");
    // The answer is the ledger reader's own: a boot would have failed
    // elsewhere first (config, catalogue), never on this path.
    let err = outcome.expect_err("a missing ledger is reported");
    assert!(
        format!("{err:#}").contains("could not open database at"),
        "{err:#}"
    );
    assert!(!socket.exists(), "no steward was booted");
}
