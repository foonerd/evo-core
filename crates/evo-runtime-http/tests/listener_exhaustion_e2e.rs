// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The HTTPS listener in a process that has run out of file
//! descriptors.
//!
//! The listener runs in a child process with a low file limit; this
//! process connects to it until its connections fill the child's file
//! table and more wait to be accepted. Each accept then fails. The
//! listener must pause between attempts rather than spin, report the
//! failures in a line or two rather than one line each, and still end
//! when stopped.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Marks the child process.
const CHILD: &str = "EVO_RUNTIME_HTTP_EXHAUSTION_CHILD";

/// The child's file limit.
const FILE_LIMIT: u32 = 128;

/// Connections left waiting in the child's listen queue once its file
/// table is full.
const QUEUED: usize = 16;

/// How long the child's accepts are left failing.
const WINDOW: Duration = Duration::from_secs(3);

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

#[test]
fn an_exhausted_listener_pauses_reports_sparingly_and_still_stops() {
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("ulimit -n {FILE_LIMIT} && exec \"$0\" \"$@\""))
        .arg(&exe)
        .args(["--exact", "child", "--ignored", "--nocapture"])
        .args(["--test-threads", "1"])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let addr = loop {
        let line = lines.next().expect("the child said where it listens");
        // The test harness may have started the line with the test's
        // name.
        if let Some((_, addr)) = line.unwrap().split_once("listening ") {
            break addr.trim().to_string();
        }
    };

    let held = exhaust(addr.parse().unwrap(), pid, FILE_LIMIT);
    let before = cpu_seconds(pid);
    std::thread::sleep(WINDOW);
    let used = cpu_seconds(pid) - before;

    writeln!(stdin, "stop").unwrap();
    let started = Instant::now();
    let mut report = String::new();
    for line in lines {
        let line = line.unwrap();
        report.push_str(&line);
        report.push('\n');
    }
    let status = child.wait().unwrap();
    let took = started.elapsed();
    let connected = held.len();
    drop(held);
    eprintln!(
        "{connected} connections; {used:.2} CPU seconds in {WINDOW:?}; \
         stopped in {took:?}\n{report}"
    );

    assert!(status.success(), "the child failed:\n{report}");
    assert!(
        took < Duration::from_secs(10),
        "the child took {took:?} to stop"
    );
    assert!(
        used < 1.0,
        "the child used {used:.2} CPU seconds in {WINDOW:?} of failing \
         accepts:\n{report}"
    );
}

/// The listener side, run in its own process under the parent's file
/// limit: listens, reports the address, stops when told, and checks
/// what it logged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run in a child process by the test above"]
async fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    counting::install();

    let ca = evo_tls_certs::generate_device_ca(
        &evo_tls_certs::DeviceCaConfig::default(),
    )
    .unwrap();
    let bundle = ca
        .issue_leaf(&evo_tls_certs::LeafConfig::for_hostnames(vec![
            "localhost".to_string(),
        ]))
        .unwrap();
    // Bounds wide enough that the file table, not the listener, runs
    // out first.
    let config = evo_runtime_http::HttpsListenerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
    )
    .with_handshake_timeout(Duration::from_secs(600))
    .with_max_incomplete_connections(
        std::num::NonZeroUsize::new(4096).unwrap(),
    );
    let (handle, join) = evo_runtime_http::serve_https(
        config,
        evo_runtime_http::Router::new(),
        &bundle,
    )
    .await
    .unwrap();
    println!("listening {}", handle.local_addr);

    // Stop from another thread when the parent says so, as a signal
    // handler would.
    let shutdown = std::sync::Arc::clone(&handle.shutdown);
    let (told, stop_sent) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        shutdown.notify_waiters();
        let _ = told.send(());
    });

    stop_sent.await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(2), join).await;
    let warnings = counting::warnings();
    println!("accept warnings {warnings}");
    assert!(
        ended.is_ok(),
        "the listener had not ended 2s after the stop"
    );
    assert!(
        (1..=2).contains(&warnings),
        "{warnings} warnings for one stretch of failing accepts"
    );
}

/// A subscriber that counts the listener's warnings.
mod counting {
    use std::sync::atomic::{AtomicU64, Ordering};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

    static WARNINGS: AtomicU64 = AtomicU64::new(0);

    struct Counting;

    impl Subscriber for Counting {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= &Level::INFO
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let metadata = event.metadata();
            if metadata.level() == &Level::WARN
                && metadata.target().starts_with("evo_runtime_http")
            {
                WARNINGS.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    pub fn install() {
        tracing::subscriber::set_global_default(Counting).unwrap();
    }

    pub fn warnings() -> u64 {
        WARNINGS.load(Ordering::Relaxed)
    }
}
