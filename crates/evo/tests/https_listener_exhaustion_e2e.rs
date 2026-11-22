// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The steward stops while connections exhaust its file descriptors.
//!
//! The steward runs here as its own process, started as a unit starts
//! it but under a low file limit, with its HTTPS listener bounds opened
//! wide so the file table runs out before any bound applies. This
//! process then connects until the steward's file table is full and more
//! connections wait to be accepted, so every accept fails. A SIGTERM
//! must still end the steward, every time, and the failing accepts must
//! not keep it busy.

#![cfg(target_os = "linux")]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal catalogue for the listener exhaustion pins."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echo shelf."
"#;

/// The steward's file limit.
const FILE_LIMIT: u32 = 256;

/// Connections left waiting in the steward's listen queue once its
/// file table is full.
const QUEUED: usize = 16;

/// How long a stopped steward may take to end.
const STOP_WITHIN: Duration = Duration::from_secs(10);

/// A directory with a short path: a socket's path may not be longer
/// than 107 bytes, whatever the temporary directory of the day is.
fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("evo-exh")
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

/// CPU seconds a process has used, from `/proc/<pid>/stat`.
fn cpu_seconds(pid: u32) -> f64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // Fields after the parenthesised name, starting at the state
    // (field 3); utime and stime are fields 14 and 15.
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    let ticks: u64 =
        fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    let per_second: u64 = String::from_utf8(
        Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .parse()
    .unwrap();
    ticks as f64 / per_second as f64
}

/// Files a process holds open.
fn files_open(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("the file table")
        .count()
}

/// Connect to `addr` until the process `pid` holds `limit` files, then
/// queue [`QUEUED`] more, each of which it fails to accept. The
/// connections stay open as long as the returned handles do.
fn exhaust(addr: SocketAddr, pid: u32, limit: u32) -> Vec<TcpStream> {
    let mut held = Vec::new();
    let until = Instant::now() + Duration::from_secs(30);
    while files_open(pid) < limit as usize {
        assert!(
            Instant::now() < until,
            "{} files of {limit} open after {} connections",
            files_open(pid),
            held.len()
        );
        // A full listen queue drops the attempt; the next one retries
        // once the process has caught up.
        match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            Ok(connection) => held.push(connection),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    for _ in 0..QUEUED {
        if let Ok(connection) =
            TcpStream::connect_timeout(&addr, Duration::from_millis(200))
        {
            held.push(connection);
        }
    }
    // Let the failing accepts begin.
    std::thread::sleep(Duration::from_millis(300));
    held
}

/// The steward binary under a low file limit, over one state directory.
struct Steward {
    child: Child,
    listener: SocketAddr,
}

impl Steward {
    /// Start the steward over `dir`, or start it again over what an
    /// earlier start left there.
    fn start(dir: &Path) -> Self {
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
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("ulimit -n {FILE_LIMIT} && exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_evo"))
            .arg("--config")
            .arg(&config)
            .env("EVO_HTTPS_LISTEN_ADDR", format!("127.0.0.1:{port}"))
            // Wide enough that the file table runs out first.
            .env("EVO_HTTPS_MAX_INCOMPLETE_CONNECTIONS", "4096")
            .env("EVO_HTTPS_HANDSHAKE_TIMEOUT_SECS", "600")
            .env_remove("EVO_HTTPS_STATE_DIR")
            .env_remove("EVO_AUTH_TIER")
            .env_remove("EVO_HTTP_REDIRECT_LISTEN_ADDR")
            .env_remove("EVO_MTLS_CLIENT_CA_FILE")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the steward");

        // Up when its listener takes a connection.
        let listener = SocketAddr::from(([127, 0, 0, 1], port));
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!("the steward ended at once: {status}");
            }
            if dir.join("state/https/ca.crt").exists()
                && TcpStream::connect_timeout(
                    &listener,
                    Duration::from_millis(200),
                )
                .is_ok()
            {
                break;
            }
            assert!(Instant::now() < until, "the steward never answered");
            std::thread::sleep(Duration::from_millis(100));
        }
        Self { child, listener }
    }

    /// Fill the steward's file table and queue connections it cannot
    /// accept.
    fn exhaust(&self) -> Vec<TcpStream> {
        exhaust(self.listener, self.child.id(), FILE_LIMIT)
    }

    /// Stop the steward as its unit does. How it ended, or `None` if it
    /// had not within [`STOP_WITHIN`].
    fn stop(mut self) -> Option<Option<i32>> {
        Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill -TERM");
        let until = Instant::now() + STOP_WITHIN;
        while Instant::now() < until {
            if let Some(ended) = self.child.try_wait().expect("try_wait") {
                return Some(ended.code());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Steward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Twenty rounds: each time, a SIGTERM while every accept fails ends
/// the steward cleanly within the bound.
#[test]
fn a_sigterm_ends_the_steward_while_its_files_are_exhausted() {
    let dir = short_dir();
    let mut slowest = Duration::ZERO;
    for round in 1..=20 {
        let steward = Steward::start(dir.path());
        let held = steward.exhaust();
        let stopping = Instant::now();
        let ended = steward.stop();
        slowest = slowest.max(stopping.elapsed());
        drop(held);
        assert_eq!(
            ended,
            Some(Some(0)),
            "round {round}: the steward did not end cleanly within \
             {STOP_WITHIN:?} of SIGTERM"
        );
    }
    eprintln!("twenty rounds; the slowest stop took {slowest:?}");
}

/// Failing accepts do not keep the steward busy: five seconds of them
/// cost it well under a second of CPU.
#[test]
fn failing_accepts_do_not_keep_the_steward_busy() {
    let dir = short_dir();
    let steward = Steward::start(dir.path());
    let held = steward.exhaust();
    let before = cpu_seconds(steward.child.id());
    std::thread::sleep(Duration::from_secs(5));
    let used = cpu_seconds(steward.child.id()) - before;
    let ended = steward.stop();
    drop(held);
    eprintln!("{used:.2} CPU seconds in five seconds of failing accepts");
    assert!(
        used < 1.0,
        "the steward used {used:.2} CPU seconds in five seconds of \
         failing accepts"
    );
    assert_eq!(ended, Some(Some(0)));
}
