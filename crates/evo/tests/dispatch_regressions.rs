// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Named regressions for the concurrent-dispatch contract.
//!
//! Two tests pin the fix by name and guard the router LAYER A +
//! host LAYER B contract from silent regression:
//!
//! 1. [`reads_stay_live_during_mutation_waiting_on_prompt`] —
//!    a mutation on shelf S that awaits an external signal
//!    (a stand-in for the credential prompt in the shares plugin)
//!    must NOT block a peer read on the same shelf. Under the
//!    router's RwLock read-guard dispatch (LAYER A) the read
//!    proceeds while the mutation is still awaiting; a
//!    regression to a per-entry mutex held across await would
//!    trip the assertion.
//!
//! 2. [`cancel_wakes_waiters_with_concurrent_queued_read`] —
//!    two mutations (A, B) both await the same external signal
//!    while a read (L) is in flight on the same shelf. Cancelling
//!    the signal wakes A and B; L must have already returned
//!    while A and B were awaiting. Under the router LAYER A +
//!    host LAYER B contract (spawn per HandleRequest against
//!    the plugin read guard) all three complete within a
//!    bounded window.
//!
//! Both tests use a `SignalRespondent` fixture — a plugin that
//! exposes:
//!   - `long_write`: awaits `wait_notify.notified().await`, then
//!     returns success. Every concurrent caller waits on the same
//!     `Notify`; `notify_waiters()` releases all pending.
//!   - `quick_read`: returns immediately.
//!
//! The fixture's `wait_notify` stands in for the framework's
//! credential prompt. The tests exercise the router + host
//! dispatch paths directly through the Unix socket, matching the
//! shape of `concurrency.rs`'s cross-shelf overlap proof.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
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

/// One-shelf catalogue on the `signals` rack.
const SIGNAL_CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "signals"
family = "domain"
charter = "One-shelf rack used by the concurrent-dispatch regression tests."

[[racks.shelves]]
name = "hub"
shape = 1
description = "Single respondent shelf for LAYER A / LAYER B regressions."
"#;

/// Bounded wall-clock ceiling within which a `quick_read` must
/// return while a `long_write` is still awaiting the shared
/// `Notify`. Chosen loose enough to survive CI scheduler jitter
/// but well below any value that could accidentally pass under
/// sequential dispatch.
const QUICK_READ_CEILING: Duration = Duration::from_millis(500);

/// Bounded wall-clock ceiling within which both cancelled
/// `long_write` mutations must resolve after the shared
/// `Notify::notify_waiters()` fires while a `quick_read` is in
/// flight. Same rationale as `QUICK_READ_CEILING`.
const CANCEL_WAKE_CEILING: Duration = Duration::from_millis(3_000);

/// Respondent plugin used by both regression tests. `long_write`
/// awaits `wait_notify.notified().await`; `quick_read` returns
/// immediately. Concurrent `long_write` callers all wait on the
/// same `Notify`; a single `notify_waiters()` releases every
/// pending caller — the stand-in for a credential-prompt cancel.
struct SignalRespondent {
    name: String,
    wait_notify: Arc<tokio::sync::Notify>,
    loaded: bool,
}

impl Plugin for SignalRespondent {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: self.name.clone(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        "long_write".to_string(),
                        "quick_read".to_string(),
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
        async move {
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move { HealthReport::healthy() }
    }
}

impl Respondent for SignalRespondent {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        let wait_notify = Arc::clone(&self.wait_notify);
        let verb = req.request_type.clone();
        async move {
            match verb.as_str() {
                "quick_read" => {
                    Ok(Response::for_request(req, b"read-ok".to_vec()))
                }
                "long_write" => {
                    wait_notify.notified().await;
                    Ok(Response::for_request(req, b"write-cancelled".to_vec()))
                }
                other => Err(PluginError::Permanent(format!(
                    "unknown verb: {other}"
                ))),
            }
        }
    }
}

fn signal_manifest(plugin_name: &str) -> Manifest {
    let toml = format!(
        r#"
[plugin]
name = "{plugin_name}"
version = "0.1.0"
contract = 1

[target]
shelf = "signals.hub"
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
request_types = ["long_write", "quick_read"]
response_budget_ms = 30000
"#
    );
    Manifest::from_toml(&toml).expect("signal manifest must parse")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_stay_live_during_mutation_waiting_on_prompt() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let catalogue = Arc::new(
        Catalogue::from_toml(SIGNAL_CATALOGUE_TOML)
            .expect("signal catalogue parses"),
    );
    let state = StewardState::builder()
        .catalogue(catalogue)
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .persistence(Arc::new(MemoryPersistenceStore::new()))
        .claimant_issuer(Arc::new(evo::claimant::ClaimantTokenIssuer::new(
            "test-instance",
        )))
        .build()
        .expect("steward state must build");

    let mut engine = AdmissionEngine::new(
        Arc::clone(&state),
        std::path::PathBuf::from(
            "/tmp/evo-reads-live-during-mutation-test-data-root",
        ),
        std::path::PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    );

    let wait_notify = Arc::new(tokio::sync::Notify::new());
    let plugin = SignalRespondent {
        name: "org.test.signal.reads-live".into(),
        wait_notify: Arc::clone(&wait_notify),
        loaded: false,
    };
    engine
        .admit_singleton_respondent(
            plugin,
            signal_manifest("org.test.signal.reads-live"),
        )
        .await
        .expect("admit signal respondent");

    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let router = Arc::clone(engine.router());
    let server = Server::new(
        socket_path.clone(),
        router,
        Arc::clone(&state),
        Arc::clone(&projections),
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        server
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("server run");
    });

    wait_for_socket(&socket_path, Duration::from_secs(2))
        .await
        .expect("server socket must become available");

    // Step 1: fire long_write on connection W. Its handler will
    // await wait_notify.notified() and not return until we call
    // notify_waiters() below.
    let mut stream_w = UnixStream::connect(&socket_path)
        .await
        .expect("connect write client");
    let long_write_req = r#"{"op":"request","shelf":"signals.hub","request_type":"long_write","payload_b64":""}"#;
    write_frame(&mut stream_w, long_write_req.as_bytes()).await;
    // Do not read the response yet — the handler is intentionally
    // still awaiting.

    // Give the mutation a moment to enter its handler + acquire
    // the router's read guard on the plugin.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Step 2: fire quick_read on connection L. Under LAYER A
    // retired the read acquires its own read guard on the same
    // plugin (concurrent with the mutation's read guard) and
    // returns immediately. Pre-fix the read would have queued on
    // entry.handle: AsyncMutex and only returned after the
    // mutation completed — which under this test never happens
    // until we notify_waiters().
    let mut stream_l = UnixStream::connect(&socket_path)
        .await
        .expect("connect read client");
    let quick_read_req = r#"{"op":"request","shelf":"signals.hub","request_type":"quick_read","payload_b64":""}"#;

    let read_start = Instant::now();
    write_frame(&mut stream_l, quick_read_req.as_bytes()).await;
    let read_resp = read_frame(&mut stream_l).await;
    let read_elapsed = read_start.elapsed();

    // Assertion: read returned promptly, well under the bounded
    // ceiling. If this fires, LAYER A has regressed: the router
    // is again holding an exclusive lock on entry.handle across
    // the mutation's await.
    assert!(
        read_elapsed < QUICK_READ_CEILING,
        "quick_read took {read_elapsed:?} while a long_write \
         held the shelf; expected under {QUICK_READ_CEILING:?}. \
         LAYER A appears to have regressed — the router is \
         serialising handle_request on entry.handle across the \
         mutation's await."
    );

    // The read must have succeeded, not returned an error.
    let read_v: serde_json::Value =
        serde_json::from_slice(&read_resp).expect("read resp JSON");
    let read_payload = read_v
        .get("payload_b64")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("read resp missing payload_b64: {read_v}"));
    assert_eq!(
        B64.decode(read_payload).expect("decode read payload"),
        b"read-ok"
    );

    // Cleanup: release the mutation and drain its response so the
    // server's per-connection dispatcher can exit cleanly.
    wait_notify.notify_waiters();
    let _write_resp = read_frame(&mut stream_w).await;

    drop(stream_w);
    drop(stream_l);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task join");
    engine.shutdown().await.expect("drain admission engine");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_wakes_waiters_with_concurrent_queued_read() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let catalogue = Arc::new(
        Catalogue::from_toml(SIGNAL_CATALOGUE_TOML)
            .expect("signal catalogue parses"),
    );
    let state = StewardState::builder()
        .catalogue(catalogue)
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .persistence(Arc::new(MemoryPersistenceStore::new()))
        .claimant_issuer(Arc::new(evo::claimant::ClaimantTokenIssuer::new(
            "test-instance",
        )))
        .build()
        .expect("steward state must build");

    let mut engine = AdmissionEngine::new(
        Arc::clone(&state),
        std::path::PathBuf::from(
            "/tmp/evo-cancel-wakes-waiters-test-data-root",
        ),
        std::path::PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    );

    let wait_notify = Arc::new(tokio::sync::Notify::new());
    let plugin = SignalRespondent {
        name: "org.test.signal.cancel-wakes".into(),
        wait_notify: Arc::clone(&wait_notify),
        loaded: false,
    };
    engine
        .admit_singleton_respondent(
            plugin,
            signal_manifest("org.test.signal.cancel-wakes"),
        )
        .await
        .expect("admit signal respondent");

    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let router = Arc::clone(engine.router());
    let server = Server::new(
        socket_path.clone(),
        router,
        Arc::clone(&state),
        Arc::clone(&projections),
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        server
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("server run");
    });

    wait_for_socket(&socket_path, Duration::from_secs(2))
        .await
        .expect("server socket must become available");

    // Step 1: fire long_write on connections A and B. Both
    // handlers will await wait_notify.notified() and stay
    // pending until we call notify_waiters().
    let mut stream_a =
        UnixStream::connect(&socket_path).await.expect("connect A");
    let mut stream_b =
        UnixStream::connect(&socket_path).await.expect("connect B");
    let long_write_req = r#"{"op":"request","shelf":"signals.hub","request_type":"long_write","payload_b64":""}"#;
    write_frame(&mut stream_a, long_write_req.as_bytes()).await;
    write_frame(&mut stream_b, long_write_req.as_bytes()).await;

    // Give A and B a moment to enter their handlers and start
    // awaiting the shared Notify.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Step 2: fire quick_read on connection L. Under Sessions B
    // + C dispatch, L acquires its own read guard on the plugin
    // concurrent with A's and B's read guards; L returns
    // immediately. Pre-fix the reader-block deadlock class would
    // have kept L queued behind the plugin-side sequential
    // dispatch, and cancel would never have woken A or B.
    let mut stream_l =
        UnixStream::connect(&socket_path).await.expect("connect L");
    let quick_read_req = r#"{"op":"request","shelf":"signals.hub","request_type":"quick_read","payload_b64":""}"#;

    let read_start = Instant::now();
    write_frame(&mut stream_l, quick_read_req.as_bytes()).await;
    let read_resp = read_frame(&mut stream_l).await;
    let read_elapsed = read_start.elapsed();

    assert!(
        read_elapsed < QUICK_READ_CEILING,
        "quick_read took {read_elapsed:?} with two long_writes \
         holding the shelf; expected under {QUICK_READ_CEILING:?}. \
         LAYER A or LAYER B has regressed."
    );
    let read_v: serde_json::Value =
        serde_json::from_slice(&read_resp).expect("read resp JSON");
    assert!(
        read_v.get("payload_b64").is_some(),
        "read response missing payload_b64: {read_v}"
    );

    // Step 3: cancel — release both awaiting mutations. Under a
    // healthy dispatch A and B both wake and return within
    // CANCEL_WAKE_CEILING. Pre-fix (reader-block deadlock class)
    // A and B never resolved — shelf remained dead until service
    // restart.
    let cancel_start = Instant::now();
    wait_notify.notify_waiters();
    let (a_resp, b_resp) = tokio::time::timeout(CANCEL_WAKE_CEILING, async {
        let a = read_frame(&mut stream_a).await;
        let b = read_frame(&mut stream_b).await;
        (a, b)
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "cancel_wakes_waiters: A and B did not resolve within \
             {CANCEL_WAKE_CEILING:?} of notify_waiters(); the \
             concurrent-dispatch contract has regressed — reader-\
             block deadlock class is back"
        )
    });
    let cancel_wake_elapsed = cancel_start.elapsed();

    let a_v: serde_json::Value =
        serde_json::from_slice(&a_resp).expect("A resp JSON");
    let b_v: serde_json::Value =
        serde_json::from_slice(&b_resp).expect("B resp JSON");
    let a_payload = a_v
        .get("payload_b64")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("A resp missing payload_b64: {a_v}"));
    let b_payload = b_v
        .get("payload_b64")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("B resp missing payload_b64: {b_v}"));
    assert_eq!(
        B64.decode(a_payload).expect("decode A payload"),
        b"write-cancelled"
    );
    assert_eq!(
        B64.decode(b_payload).expect("decode B payload"),
        b"write-cancelled"
    );
    assert!(
        cancel_wake_elapsed < CANCEL_WAKE_CEILING,
        "cancel-wake took {cancel_wake_elapsed:?}; expected under \
         {CANCEL_WAKE_CEILING:?}"
    );

    drop(stream_a);
    drop(stream_b);
    drop(stream_l);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task join");
    engine.shutdown().await.expect("drain admission engine");
}

// ---------------------------------------------------------------------
// Local Unix-socket framing helpers (kept file-local to avoid
// coupling the regression tests to the concurrency.rs harness).
// ---------------------------------------------------------------------

async fn write_frame(stream: &mut UnixStream, body: &[u8]) {
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).await.expect("write len");
    stream.write_all(body).await.expect("write body");
    stream.flush().await.expect("flush");
}

async fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.expect("read len");
    let len = u32::from_be_bytes(len_buf) as usize;
    assert!(len > 0, "response length must be non-zero");
    assert!(
        len < 1024 * 1024,
        "response length suspiciously large: {len}"
    );
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("read body");
    body
}

async fn wait_for_socket(
    path: &std::path::Path,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() && UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "socket {} did not become available within {:?}",
                path.display(),
                timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
