// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Revocations outlive a restart, wherever the boot keeps them, a
//! reset retires every bearer at the next start, and every bearer the
//! steward issues is in its inventory, where it can be revoked.
//!
//! The steward runs here as its own process, started as a unit starts
//! it and without `EVO_HTTPS_STATE_DIR`, so its HTTPS material sits
//! beside the persistence file. A revocation made over the wire must be
//! written where the next boot reads the revocation list, and the reset
//! gesture must clear the list that boot reads. The start after a reset
//! replaces the bearer-token signing key before it loads one, so no
//! bearer issued before it is taken again, revoked or not. The reset
//! also holds the auth tier at `Open`, at once and at every start,
//! until `evo auth restore-tier` runs with the steward stopped.
//!
//! Everything goes through a public path: the steward binary, its
//! HTTPS listener, its local socket, and the bearers it mints itself.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{json, Value};

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal catalogue for the revocation restart pins."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echo shelf."
"#;

/// The first-boot pairing code the pins write to `preseed`.
const PRESEED: &str = "pin-preseed-5517";

/// A directory with a short path: a socket's path may not be longer
/// than 107 bytes, whatever the temporary directory of the day is.
fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("evo-rev")
        .tempdir_in("/tmp")
        .expect("tempdir")
}

/// A port nothing listens on.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("local addr")
        .port()
}

/// The steward binary, running over one state directory.
struct Steward {
    child: Child,
    dir: PathBuf,
    port: u16,
    web: reqwest::Client,
}

impl Steward {
    /// Start the steward over `dir`, or start it again over what an
    /// earlier start left there, and wait for its HTTPS listener. The
    /// persistence file is `dir/state/evo.db`, so the boot keeps its
    /// HTTPS material under `dir/state/https`.
    async fn start(dir: &Path) -> Self {
        let (mut child, port) = Self::launch(dir);

        // Up when its own local socket answers and its listener takes
        // a connection. The steward binds the socket only after its
        // HTTPS boot, so a port another test's steward took in the
        // meantime cannot pass for this one; the certificate authority
        // is on disk by then.
        let authority = dir.join("state/https/ca.crt");
        let socket = dir.join("evo.sock");
        let listener = SocketAddr::from(([127, 0, 0, 1], port));
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!("the steward ended at once: {status}");
            }
            if authority.exists()
                && UnixStream::connect(&socket).is_ok()
                && TcpStream::connect_timeout(
                    &listener,
                    Duration::from_millis(200),
                )
                .is_ok()
            {
                break;
            }
            assert!(Instant::now() < until, "the steward never answered");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Self::attach(child, dir, port)
    }

    /// Start the steward again over `dir` and wait for its local
    /// socket only. The steward binds that socket after its HTTPS boot
    /// has run, so once it answers the HTTPS listener is up or never
    /// will be.
    async fn start_local(dir: &Path) -> Self {
        let (mut child, port) = Self::launch(dir);
        let socket = dir.join("evo.sock");
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!("the steward ended at once: {status}");
            }
            if UnixStream::connect(&socket).is_ok() {
                break;
            }
            assert!(Instant::now() < until, "the steward never answered");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Self::attach(child, dir, port)
    }

    /// Whether the HTTPS listener takes a connection.
    fn listens(&self) -> bool {
        TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], self.port)),
            Duration::from_millis(500),
        )
        .is_ok()
    }

    /// Write the unit's configuration under `dir` and spawn the
    /// steward on a free port.
    fn launch(dir: &Path) -> (Child, u16) {
        let socket = dir.join("evo.sock");
        let config = dir.join("evo.toml");
        let catalogue = dir.join("catalogue.toml");
        std::fs::write(&catalogue, CATALOGUE_TOML).expect("catalogue");
        let at = |name: &str| dir.join(name).display().to_string();
        std::fs::write(
            &config,
            format!(
                r#"
[steward]
log_level = "warn"
socket_path = "{socket}"

[catalogue]
path = "{catalogue}"
lkg_path = "{lkg}"

[persistence]
path = "{db}"

[plugins]
allow_unsigned = true
plugin_data_root = "{data}"
runtime_dir = "{runtime}"
config_dir = "{plugins_d}"
search_roots = []
bundled_roots = []
trust_dir_opt = "{trust_opt}"
trust_dir_etc = "{trust_etc}"
revocations_path = "{revocations}"

[plugins.stage]
dir = "{stage}"

[plans]
storage_root = "{plans}"

[multiroom]
discovery_enabled = false
"#,
                socket = socket.display(),
                catalogue = catalogue.display(),
                lkg = at("state/catalogue.lkg.toml"),
                db = at("state/evo.db"),
                data = at("plugins-data"),
                runtime = at("plugins-run"),
                plugins_d = at("plugins.d"),
                trust_opt = at("trust-opt"),
                trust_etc = at("trust-etc"),
                revocations = at("revocations.toml"),
                stage = at("stage"),
                plans = at("plans"),
            ),
        )
        .expect("config");
        std::fs::create_dir_all(dir.join("state")).expect("state dir");

        let port = free_port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_evo"));
        // The unit's configured tier, when a pin wrote one.
        match std::fs::read_to_string(dir.join("auth-tier")) {
            Ok(tier) => command.env("EVO_AUTH_TIER", tier.trim()),
            Err(_) => command.env_remove("EVO_AUTH_TIER"),
        };
        let child = command
            .arg("--config")
            .arg(&config)
            .env("EVO_HTTPS_LISTEN_ADDR", format!("127.0.0.1:{port}"))
            // A browser pairs with this code when the file exists.
            .env("EVO_PAIR_PRESEED_FILE", dir.join("preseed"))
            .env_remove("EVO_HTTPS_STATE_DIR")
            .env_remove("EVO_HTTP_REDIRECT_LISTEN_ADDR")
            .env_remove("EVO_MTLS_CLIENT_CA_FILE")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the steward");
        (child, port)
    }

    /// The steward started over `dir`, with a client that trusts the
    /// certificate authority its first start made.
    fn attach(child: Child, dir: &Path, port: u16) -> Self {
        let authority = dir.join("state/https/ca.crt");
        let ca = std::fs::read(&authority).expect("ca.crt");
        let web = reqwest::Client::builder()
            .add_root_certificate(
                reqwest::Certificate::from_pem(&ca).expect("ca pem"),
            )
            .tls_built_in_root_certs(false)
            .danger_accept_invalid_hostnames(true)
            .pool_max_idle_per_host(0)
            .build()
            .expect("client");
        Self {
            child,
            dir: dir.to_path_buf(),
            port,
            web,
        }
    }

    /// Where the boot keeps the HTTPS material for this unit.
    fn https_dir(&self) -> PathBuf {
        self.dir.join("state").join("https")
    }

    /// The bearer the steward made for its operator when it first
    /// started.
    fn operator(&self) -> String {
        std::fs::read_to_string(self.https_dir().join("bootstrap.token"))
            .expect("bootstrap.token")
            .trim()
            .to_string()
    }

    /// One of the steward's operations over HTTPS, presenting `bearer`,
    /// or nothing when `bearer` is empty.
    async fn op(
        &self,
        method: Method,
        name: &str,
        body: Option<Value>,
        bearer: &str,
    ) -> (u16, Value) {
        let mut asked = self.web.request(
            method,
            format!("https://127.0.0.1:{}/api/v1/{name}", self.port),
        );
        if let Some(body) = body {
            asked = asked.json(&body);
        }
        if !bearer.is_empty() {
            asked = asked.bearer_auth(bearer);
        }
        let answer =
            asked.send().await.expect("the steward answers over HTTPS");
        let status = answer.status().as_u16();
        let text = answer.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// The status of a read asked with no bearer from a public
    /// address. The tier decides it: `Secure` refuses such a caller,
    /// `Open` admits it.
    async fn public_read(&self) -> u16 {
        self.web
            .get(format!(
                "https://127.0.0.1:{}/api/v1/_observatory/recent",
                self.port
            ))
            .header("x-forwarded-for", "203.0.113.7")
            .send()
            .await
            .expect("the steward answers over HTTPS")
            .status()
            .as_u16()
    }

    /// `evo auth restore-tier` over the unit's state under `dir`, with
    /// its configuration and socket. Whether it succeeded, and what it
    /// printed.
    fn restore_tier(dir: &Path) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_evo"))
            .arg("--config")
            .arg(dir.join("evo.toml"))
            .arg("--socket")
            .arg(dir.join("evo.sock"))
            .args(["auth", "restore-tier", "--state-dir"])
            .arg(dir.join("state"))
            .env_remove("EVO_HTTPS_STATE_DIR")
            .output()
            .expect("run evo auth restore-tier");
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    /// Whether the steward takes `bearer`: the status of an operation
    /// that needs one.
    async fn takes(&self, bearer: &str) -> u16 {
        self.op(Method::GET, "list_bearer_tokens", None, bearer)
            .await
            .0
    }

    /// Mint a bearer that reads the credential inventory. Its id and
    /// the bearer.
    async fn mint(&self, operator: &str) -> (String, String) {
        let (status, minted) = self
            .op(
                Method::POST,
                "create_bearer_token",
                Some(json!({
                    "name": "held",
                    "reason": "revocation pin",
                    "scopes": [{"kind": "read", "scope": "auth"}],
                })),
                operator,
            )
            .await;
        assert_eq!(status, 200, "{minted}");
        (
            minted["record"]["token_id"]
                .as_str()
                .expect("token_id")
                .to_string(),
            minted["token"].as_str().expect("token").to_string(),
        )
    }

    async fn revoke(&self, operator: &str, token_id: &str) {
        let (status, body) = self
            .op(
                Method::DELETE,
                "revoke_bearer_token",
                Some(json!({"token_id": token_id, "reason": "revocation pin"})),
                operator,
            )
            .await;
        assert_eq!(status, 200, "{body}");
    }

    /// The reset gesture, asked by `operator`.
    async fn reset(&self, operator: &str) {
        let (status, body) = self
            .op(
                Method::POST,
                "reset_credentials_to_open",
                Some(json!({"reason": "reset pin"})),
                operator,
            )
            .await;
        assert_eq!(status, 200, "{body}");
    }

    /// The names in the credential inventory, read with `bearer`.
    async fn records(&self, bearer: &str) -> Vec<String> {
        let (status, body) = self
            .op(Method::GET, "list_bearer_tokens", None, bearer)
            .await;
        assert_eq!(status, 200, "{body}");
        let mut names: Vec<String> = body["records"]
            .as_array()
            .expect("records")
            .iter()
            .map(|r| r["name"].as_str().expect("name").to_string())
            .collect();
        names.sort();
        names
    }

    /// Every record in the inventory, read with `bearer`.
    async fn inventory(&self, bearer: &str) -> Vec<Value> {
        let (status, body) = self
            .op(Method::GET, "list_bearer_tokens", None, bearer)
            .await;
        assert_eq!(status, 200, "{body}");
        body["records"].as_array().expect("records").clone()
    }

    /// The record for `token_id`, if the inventory holds one.
    async fn record(&self, bearer: &str, token_id: &str) -> Option<Value> {
        self.inventory(bearer)
            .await
            .into_iter()
            .find(|r| r["token_id"] == token_id)
    }

    /// Pair a browser with the preseed code, as a first-boot browser
    /// does. Its bearer, the bearer's id and the paired device's id.
    async fn pair(&self) -> (String, String, String) {
        let (status, paired) = self
            .op(
                Method::POST,
                "pair_complete",
                Some(json!({"pair_id": "bootstrap", "code": PRESEED})),
                "",
            )
            .await;
        assert_eq!(status, 200, "{}", paired["error"]);
        (
            paired["token"].as_str().expect("token").to_string(),
            paired["token_id"].as_str().expect("token_id").to_string(),
            paired["paired_device_id"]
                .as_str()
                .expect("paired_device_id")
                .to_string(),
        )
    }

    /// One operation on the steward's local socket, after negotiating
    /// `plugins_admin`, as the operator's tools do.
    fn local(&self, request: Value) -> Value {
        use std::io::{Read, Write};
        let mut socket =
            UnixStream::connect(self.dir.join("evo.sock")).expect("socket");
        let mut call = |frame: &Value| -> Value {
            let bytes = serde_json::to_vec(frame).expect("frame");
            socket
                .write_all(&(bytes.len() as u32).to_be_bytes())
                .expect("write");
            socket.write_all(&bytes).expect("write");
            let mut len = [0u8; 4];
            socket.read_exact(&mut len).expect("read");
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            socket.read_exact(&mut body).expect("read");
            serde_json::from_slice(&body).expect("answer")
        };
        let granted = call(
            &json!({"op": "negotiate", "capabilities": ["plugins_admin"]}),
        );
        assert!(granted.to_string().contains("plugins_admin"), "{granted}");
        call(&request)
    }

    /// Stop the steward as its unit does, and wait for it to end.
    async fn stop(mut self) -> Option<i32> {
        Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill -TERM");
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(ended) = self.child.try_wait().expect("try_wait") {
                return ended.code();
            }
            assert!(
                Instant::now() < until,
                "the steward was told to stop, and had not ended a minute on"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Steward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A bearer revoked over the wire is refused at once, and still
/// refused after the steward restarts.
#[tokio::test]
async fn a_revocation_survives_a_restart() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let (token_id, held) = steward.mint(&operator).await;
    assert_eq!(steward.takes(&held).await, 200);

    steward.revoke(&operator, &token_id).await;
    assert_eq!(steward.takes(&held).await, 401, "refused at once");
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert_eq!(
        steward.takes(&held).await,
        401,
        "the revocation outlived the restart"
    );
    assert_eq!(steward.takes(&operator).await, 200);
    assert_eq!(steward.stop().await, Some(0));
}

/// The reset gesture clears the revocation list the boot reads, and
/// says how many revocations it cleared.
#[tokio::test]
async fn the_reset_clears_the_revocations_the_boot_reads() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let (token_id, _held) = steward.mint(&operator).await;
    steward.revoke(&operator, &token_id).await;
    assert_eq!(steward.stop().await, Some(0));

    // Across a restart, so the list is the one the boot read.
    let steward = Steward::start(dir.path()).await;
    let (status, body) = steward
        .op(
            Method::POST,
            "reset_credentials_to_open",
            Some(json!({"reason": "reset pin"})),
            &operator,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revocations_cleared"], 1, "{body}");
    assert!(
        !steward.https_dir().join("revoked.json").exists(),
        "the list the boot reads is gone"
    );
    assert_eq!(steward.stop().await, Some(0));
}

/// A bearer revoked before the reset is still refused after the start
/// that follows it, although the reset cleared the revocation list.
#[tokio::test]
async fn a_bearer_revoked_before_the_reset_stays_refused() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let (token_id, revoked) = steward.mint(&operator).await;
    steward.revoke(&operator, &token_id).await;
    steward.reset(&operator).await;
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert_eq!(
        steward.takes(&revoked).await,
        401,
        "the reset cleared the list, and the bearer stayed refused"
    );
    assert_eq!(steward.stop().await, Some(0));
}

/// A bearer never revoked is refused after the start that follows the
/// reset: the reset retires every bearer, not only the revoked ones.
#[tokio::test]
async fn a_bearer_not_revoked_is_refused_after_the_reset() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let (_token_id, held) = steward.mint(&operator).await;
    steward.reset(&operator).await;
    assert_eq!(
        steward.takes(&held).await,
        200,
        "until the next start the key it was signed with stands"
    );
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.takes(&held).await, 401);
    assert_eq!(steward.stop().await, Some(0));
}

/// The start after the reset issues a new bootstrap token, which it
/// takes, and refuses the one from before the reset.
#[tokio::test]
async fn the_start_after_the_reset_issues_a_new_bootstrap_token() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let before = steward.operator();
    steward.reset(&before).await;
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    let after = steward.operator();
    // Compared without printing either: a failure shows no bearer.
    assert!(after != before, "a new bootstrap.token");
    assert_eq!(steward.takes(&after).await, 200);
    assert_eq!(steward.takes(&before).await, 401);
    assert_eq!(steward.records(&after).await, vec!["bootstrap"]);
    assert_eq!(steward.stop().await, Some(0));
}

/// A bearer minted after the reset and before the next start is
/// refused by that start, and its record goes with it.
#[tokio::test]
async fn a_bearer_minted_between_the_reset_and_the_start_is_refused() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    steward.reset(&operator).await;
    let (_token_id, between) = steward.mint(&operator).await;
    assert_eq!(steward.records(&operator).await, vec!["held"]);
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.takes(&between).await, 401);
    assert_eq!(
        steward.records(&steward.operator()).await,
        vec!["bootstrap"],
        "no record of a bearer the start retired"
    );
    assert_eq!(steward.stop().await, Some(0));
}

/// The reset retires the key once. A second start keeps the bootstrap
/// token and the bearers issued after the first.
#[tokio::test]
async fn the_key_is_retired_once() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    steward.reset(&steward.operator()).await;
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let (_token_id, after) = steward.mint(&operator).await;
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert!(steward.operator() == operator, "the same bootstrap.token");
    assert_eq!(steward.takes(&operator).await, 200);
    assert_eq!(steward.takes(&after).await, 200);
    assert_eq!(steward.stop().await, Some(0));
}

/// A start that cannot replace the key the reset retired does not load
/// it: no HTTPS listener comes up to take the bearers it signed. The
/// next start that can replace it does, and retires them.
#[tokio::test]
async fn a_start_that_cannot_replace_the_retired_key_does_not_load_it() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let https = steward.https_dir();
    steward.reset(&operator).await;
    assert_eq!(steward.stop().await, Some(0));

    // The new key is written beside the old one under this name before
    // it is renamed over it; a directory there cannot be replaced.
    let in_the_way = https.join("tokens.key.rotating");
    std::fs::create_dir(&in_the_way).expect("block the new key");
    let steward = Steward::start_local(dir.path()).await;
    assert!(!steward.listens(), "no HTTPS listener on the retired key");
    assert_eq!(steward.stop().await, Some(0));

    std::fs::remove_dir(&in_the_way).expect("unblock");
    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.takes(&operator).await, 401);
    assert_eq!(steward.takes(&steward.operator()).await, 200);
    assert_eq!(steward.stop().await, Some(0));
}

/// Write the first-boot pairing code where the steward reads it.
fn write_preseed(dir: &Path) {
    std::fs::write(dir.join("preseed"), format!("{PRESEED}\n"))
        .expect("preseed");
}

/// A bearer issued to a browser by pairing is in the inventory, with
/// the device it was paired to.
#[tokio::test]
async fn a_paired_bearer_is_in_the_inventory() {
    let dir = short_dir();
    write_preseed(dir.path());
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let (paired, token_id, device) = steward.pair().await;
    assert_eq!(steward.takes(&paired).await, 200);

    let record = steward
        .record(&operator, &token_id)
        .await
        .expect("a record for the paired bearer");
    assert_eq!(record["source"], "paired", "{record}");
    assert_eq!(record["paired_device_id"], device.as_str(), "{record}");
    assert_eq!(record["name"], "First-boot browser", "{record}");
    assert!(record["expires_at_ms"].as_u64().is_some(), "{record}");
    assert_eq!(steward.stop().await, Some(0));
}

/// A paired bearer is revoked by its id like any other: refused at
/// once and after a restart.
#[tokio::test]
async fn a_paired_bearer_can_be_revoked_by_its_id() {
    let dir = short_dir();
    write_preseed(dir.path());
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let (paired, token_id, _device) = steward.pair().await;
    steward.revoke(&operator, &token_id).await;
    assert_eq!(steward.takes(&paired).await, 401);
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.takes(&paired).await, 401);
    assert_eq!(steward.stop().await, Some(0));
}

/// Revoking a paired device retires its bearer, says so, and says so
/// only once.
#[tokio::test]
async fn pair_revoke_retires_the_devices_bearer() {
    let dir = short_dir();
    write_preseed(dir.path());
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let (paired, token_id, device) = steward.pair().await;

    let answer =
        steward.local(json!({"op": "pair_revoke", "paired_device_id": device}));
    assert_eq!(answer["revoked"], true, "{answer}");
    assert_eq!(steward.takes(&paired).await, 401, "refused at once");
    let record = steward
        .record(&operator, &token_id)
        .await
        .expect("the record stays, revoked");
    assert!(record["revoked_at_ms"].as_u64().is_some(), "{record}");
    let listed = steward.local(json!({"op": "pair_list"}));
    assert_eq!(listed["devices"], json!([]), "{listed}");
    let again =
        steward.local(json!({"op": "pair_revoke", "paired_device_id": device}));
    assert_eq!(again["revoked"], false, "{again}");
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.takes(&paired).await, 401, "and after a restart");
    assert_eq!(steward.stop().await, Some(0));
}

/// The paired devices outlive a restart, so the operator can still see
/// and revoke a browser paired before it.
#[tokio::test]
async fn the_paired_devices_outlive_a_restart() {
    let dir = short_dir();
    write_preseed(dir.path());
    let steward = Steward::start(dir.path()).await;
    let (_paired, _token_id, device) = steward.pair().await;
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    let listed = steward.local(json!({"op": "pair_list"}));
    let devices = listed["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 1, "{listed}");
    assert_eq!(devices[0]["id"], device.as_str(), "{listed}");
    assert_eq!(devices[0]["label"], "First-boot browser", "{listed}");
    assert_eq!(steward.stop().await, Some(0));
}

/// A bearer minted on the local socket is in the inventory and can be
/// revoked.
#[tokio::test]
async fn a_bearer_minted_on_the_local_socket_is_listed_and_revocable() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let minted = steward
        .local(json!({"op": "mint_bearer_token", "reason": "local mint pin"}));
    let bearer = minted["token"].as_str().expect("token").to_string();
    let token_id = minted["token_id"].as_str().expect("token_id").to_string();
    assert_eq!(steward.takes(&bearer).await, 200);

    let record = steward
        .record(&operator, &token_id)
        .await
        .expect("a record for the minted bearer");
    assert_eq!(record["source"], "minted", "{record}");
    assert_eq!(record["created_reason"], "local mint pin", "{record}");
    steward.revoke(&operator, &token_id).await;
    assert_eq!(steward.takes(&bearer).await, 401);
    assert_eq!(steward.stop().await, Some(0));
}

/// A record file whose expiry has passed, left as a stopped steward
/// would find it.
fn write_expired_record(https: &Path, token_id: &str) {
    std::fs::write(
        https.join("credentials").join(format!("{token_id}.json")),
        json!({
            "token_id": token_id,
            "name": "expired",
            "created_reason": "expiry pin",
            "scopes": [{"kind": "read", "scope": "auth"}],
            "expiry_policy": {"kind": "seconds", "value": 1},
            "created_at_ms": 1_000,
            "expires_at_ms": 2_000,
            "revoked_at_ms": null,
            "revoked_reason": null,
        })
        .to_string(),
    )
    .expect("expired record");
}

/// A start drops the records whose bearers have expired.
#[tokio::test]
async fn a_start_drops_expired_records() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let https = steward.https_dir();
    assert_eq!(steward.stop().await, Some(0));
    write_expired_record(&https, "long-gone");

    let steward = Steward::start(dir.path()).await;
    assert_eq!(
        steward.records(&steward.operator()).await,
        vec!["bootstrap"]
    );
    assert!(!https.join("credentials").join("long-gone.json").exists());
    assert_eq!(steward.stop().await, Some(0));
}

/// Adding a record drops the records whose bearers have expired.
#[tokio::test]
async fn a_new_record_drops_expired_records() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let https = steward.https_dir();
    write_expired_record(&https, "lapsed");
    steward.mint(&operator).await;

    assert_eq!(steward.records(&operator).await, vec!["bootstrap", "held"]);
    assert!(!https.join("credentials").join("lapsed.json").exists());
    assert_eq!(steward.stop().await, Some(0));
}

/// A second record added within a minute of the first does not rescan
/// the inventory: what lapsed in between stays until a later addition
/// or start drops it, so a mint does not slow as the inventory grows.
#[tokio::test]
async fn a_second_record_within_a_minute_does_not_rescan() {
    let dir = short_dir();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    let https = steward.https_dir();
    steward.mint(&operator).await;
    write_expired_record(&https, "lapsed-between");
    steward.mint(&operator).await;

    assert!(
        https
            .join("credentials")
            .join("lapsed-between.json")
            .exists(),
        "the second record rescanned the inventory"
    );
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert!(
        !https
            .join("credentials")
            .join("lapsed-between.json")
            .exists(),
        "the next start drops it"
    );
    assert_eq!(steward.stop().await, Some(0));
}

/// A reset opens a steward configured `Secure` at once: a caller with
/// no bearer from a public address, refused before the reset, is
/// admitted after it.
#[tokio::test]
async fn a_reset_opens_a_secure_steward_at_once() {
    let dir = short_dir();
    std::fs::write(dir.path().join("auth-tier"), "secure").unwrap();
    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.public_read().await, 401, "configured secure");
    let operator = steward.operator();
    steward.reset(&operator).await;
    assert_eq!(steward.public_read().await, 200, "open from the reset on");
    assert_eq!(steward.stop().await, Some(0));
}

/// The Open tier a reset left holds through the start that replaces the
/// key, and through every start after it, until `evo auth restore-tier`
/// runs with the steward stopped. The next start runs the configured
/// tier again.
#[tokio::test]
async fn the_open_tier_holds_until_the_configured_tier_is_restored() {
    let dir = short_dir();
    std::fs::write(dir.path().join("auth-tier"), "secure").unwrap();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    steward.reset(&operator).await;
    assert_eq!(steward.stop().await, Some(0));

    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.public_read().await, 200, "open after the restart");
    assert_eq!(steward.stop().await, Some(0));
    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.public_read().await, 200, "open at the next start");
    assert_eq!(steward.stop().await, Some(0));

    let (restored, said) = Steward::restore_tier(dir.path());
    assert!(restored, "{said}");
    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.public_read().await, 401, "configured secure again");
    assert_eq!(steward.stop().await, Some(0));
}

/// `evo auth restore-tier` refuses while the steward answers on its
/// socket, and the Open tier stays, now and at the next start.
#[tokio::test]
async fn the_configured_tier_is_not_restored_under_a_running_steward() {
    let dir = short_dir();
    std::fs::write(dir.path().join("auth-tier"), "secure").unwrap();
    let steward = Steward::start(dir.path()).await;
    let operator = steward.operator();
    steward.reset(&operator).await;
    let (restored, said) = Steward::restore_tier(dir.path());
    assert!(!restored, "refused while running: {said}");
    assert!(said.contains("running"), "{said}");
    assert_eq!(steward.public_read().await, 200, "still open");
    assert_eq!(steward.stop().await, Some(0));
    let steward = Steward::start(dir.path()).await;
    assert_eq!(steward.public_read().await, 200, "open at the next start");
    assert_eq!(steward.stop().await, Some(0));
}
