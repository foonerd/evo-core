// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! A steward that requires HTTPS does not run without it; one that
//! does not require it runs on its local socket alone.
//!
//! Both boot through `evo::run` over a unit's configuration, with
//! `EVO_HTTPS_LISTEN_ADDR` naming a port something else already holds,
//! so the HTTPS listener cannot bind. The steward that requires HTTPS
//! is a distribution's: `run` is called in this process with
//! `HttpsRequirement::Required`, the only `run` this test binary
//! makes, since a process installs one log subscriber. The steward
//! that does not is the shipped `evo` binary, as a unit starts it.

use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// The in-process boot reads its environment, which is process-wide.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal catalogue for the HTTPS requirement pins."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echo shelf."
"#;

/// A directory with a short path: a socket's path may not be longer
/// than 107 bytes.
fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("evo-req")
        .tempdir_in("/tmp")
        .expect("tempdir")
}

/// A port this test holds, so the steward's HTTPS listener cannot bind
/// it.
fn held_port() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").expect("bind")
}

/// Write a unit's configuration under `dir`; returns its path.
fn write_config(dir: &Path) -> PathBuf {
    let config = dir.join("evo.toml");
    let catalogue = dir.join("catalogue.toml");
    std::fs::write(&catalogue, CATALOGUE_TOML).expect("catalogue");
    std::fs::create_dir_all(dir.join("state")).expect("state dir");
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
            socket = at("evo.sock"),
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
    config
}

/// The listener variables a unit might carry, other than the address.
const OTHER_LISTENERS: &[&str] = &[
    "EVO_HTTPS_STATE_DIR",
    "EVO_HTTP_REDIRECT_LISTEN_ADDR",
    "EVO_MTLS_CLIENT_CA_FILE",
    "EVO_GRPC_LISTEN_ADDR",
    "EVO_GRAPHQL_LISTEN_ADDR",
    "EVO_HTTP3_LISTEN_ADDR",
    "EVO_AUTH_TIER",
];

#[tokio::test]
async fn a_steward_that_requires_https_does_not_run_without_it() {
    let _env = ENV_LOCK.lock().await;
    let dir = short_dir();
    let config = write_config(dir.path());
    let held = held_port();
    let port = held.local_addr().expect("local addr").port();
    std::env::set_var("EVO_HTTPS_LISTEN_ADDR", format!("127.0.0.1:{port}"));
    for name in OTHER_LISTENERS {
        std::env::remove_var(name);
    }

    let args = <evo::cli::Args as clap::Parser>::try_parse_from([
        "evo",
        "--config",
        config.to_str().expect("utf-8 path"),
    ])
    .expect("args parse");
    let opts = evo::RunOptions::from_args(args)
        .with_https_requirement(evo::HttpsRequirement::Required);
    let ended = tokio::time::timeout(Duration::from_secs(60), evo::run(opts))
        .await
        .expect("run returns rather than serving");
    std::env::remove_var("EVO_HTTPS_LISTEN_ADDR");

    let error = ended.expect_err("a required listener that cannot bind");
    assert!(
        format!("{error:#}")
            .contains("HTTPS is required and its listener did not start"),
        "{error:#}"
    );
    let socket = dir.path().join("evo.sock");
    assert!(
        UnixStream::connect(&socket).is_err(),
        "the local socket must not be served when the run ends"
    );
    drop(held);
}

#[tokio::test]
async fn the_shipped_steward_runs_on_its_local_socket_without_https() {
    let _env = ENV_LOCK.lock().await;
    let dir = short_dir();
    let config = write_config(dir.path());
    let held = held_port();
    let port = held.local_addr().expect("local addr").port();

    let mut command = Command::new(env!("CARGO_BIN_EXE_evo"));
    for name in OTHER_LISTENERS {
        command.env_remove(name);
    }
    let mut child = command
        .arg("--config")
        .arg(&config)
        .env("EVO_HTTPS_LISTEN_ADDR", format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the steward");

    let socket = dir.path().join("evo.sock");
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("the steward ended without HTTPS: {status}");
        }
        if UnixStream::connect(&socket).is_ok() {
            break;
        }
        assert!(Instant::now() < until, "the local socket never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill -TERM");
    let until = Instant::now() + Duration::from_secs(60);
    let code = loop {
        if let Some(ended) = child.try_wait().expect("try_wait") {
            break ended.code();
        }
        assert!(Instant::now() < until, "the steward did not stop");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(code, Some(0), "the steward stops cleanly");
    drop(held);
}
