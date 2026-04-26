//! Concurrency proof: dispatch through the [`PluginRouter`] does not
//! serialise on the admission-engine mutex.
//!
//! This test admits two slow respondent plugins on two different
//! shelves, then fires two concurrent client requests over the Unix
//! socket, one to each shelf. Each respondent records the wall-clock
//! instant at which its handler started and finished. The test
//! asserts the second-started handler began **before** the
//! first-started handler finished: the two handlers overlapped, which
//! is only possible if dispatch is not serialised.
//!
//! Pre-Phase-4 the server held `engine.lock().await` across each
//! handler, so the second request could not begin its handler until
//! the first returned. This test would fail under that ordering. It
//! passes once the server dispatches through the router directly,
//! since the router's lookup-clone-drop pattern releases its read
//! guard before awaiting the plugin's handler.
//!
//! The overlap-counter shape is preferred over a wall-clock budget
//! threshold because it is robust against scheduler jitter on busy
//! CI runners; the assertion is deterministic for any two
//! concurrent requests that both reach their handler bodies.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use std::future::Future;
use std::sync::atomic::{AtomicI64, Ordering};
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

/// Catalogue with two shelves on the `parallel` rack: each shelf
/// hosts one slow respondent so the test can address them
/// independently from the client side.
const CONCURRENCY_CATALOGUE_TOML: &str = r#"
schema_version = 1

[[racks]]
name = "parallel"
family = "domain"
charter = "Two-shelf rack used to prove concurrent dispatch."

[[racks.shelves]]
name = "a"
shape = 1
description = "First slow shelf."

[[racks.shelves]]
name = "b"
shape = 1
description = "Second slow shelf."
"#;

/// Sleep budget each slow respondent waits inside its handler. Long
/// enough that two sequential dispatches would clearly accumulate
/// (two of these = 200ms) but short enough to keep the test snappy.
const SLOW_HANDLER_SLEEP: Duration = Duration::from_millis(100);

/// Tracks each slow handler's start and finish timestamps as
/// elapsed-microseconds-since-`baseline`. `i64::MIN` means
/// "not yet recorded".
#[derive(Debug)]
struct OverlapTracker {
    baseline: Instant,
    a_started_us: AtomicI64,
    a_finished_us: AtomicI64,
    b_started_us: AtomicI64,
    b_finished_us: AtomicI64,
}

impl OverlapTracker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            baseline: Instant::now(),
            a_started_us: AtomicI64::new(i64::MIN),
            a_finished_us: AtomicI64::new(i64::MIN),
            b_started_us: AtomicI64::new(i64::MIN),
            b_finished_us: AtomicI64::new(i64::MIN),
        })
    }

    fn now_us(&self) -> i64 {
        self.baseline.elapsed().as_micros() as i64
    }
}

/// Slow respondent plugin: sleeps a fixed budget inside
/// `handle_request`, recording start and finish timestamps on a
/// shared [`OverlapTracker`]. `which` selects which pair of slots
/// (a or b) the plugin writes to.
struct SlowRespondent {
    name: String,
    which: char,
    tracker: Arc<OverlapTracker>,
    loaded: bool,
}

impl Plugin for SlowRespondent {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: self.name.clone(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec!["slow".to_string()],
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

impl Respondent for SlowRespondent {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        let tracker = Arc::clone(&self.tracker);
        let which = self.which;
        async move {
            let started_us = tracker.now_us();
            let (start_slot, finish_slot) = match which {
                'a' => (&tracker.a_started_us, &tracker.a_finished_us),
                'b' => (&tracker.b_started_us, &tracker.b_finished_us),
                _ => unreachable!("which must be 'a' or 'b'"),
            };
            start_slot.store(started_us, Ordering::SeqCst);

            tokio::time::sleep(SLOW_HANDLER_SLEEP).await;

            let finished_us = tracker.now_us();
            finish_slot.store(finished_us, Ordering::SeqCst);

            Ok(Response::for_request(req, req.payload.clone()))
        }
    }
}

/// Build a manifest targeting `parallel.{shelf}` for a respondent
/// named `plugin_name`.
fn slow_manifest(plugin_name: &str, shelf: &str) -> Manifest {
    let toml = format!(
        r#"
[plugin]
name = "{plugin_name}"
version = "0.1.0"
contract = 1

[target]
shelf = "parallel.{shelf}"
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
request_types = ["slow"]
response_budget_ms = 5000
"#
    );
    Manifest::from_toml(&toml).expect("slow manifest must parse")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_requests_to_different_plugins_run_in_parallel() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    // Build state and engine over a two-shelf catalogue.
    let catalogue = Arc::new(
        Catalogue::from_toml(CONCURRENCY_CATALOGUE_TOML)
            .expect("two-shelf catalogue parses"),
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
        std::path::PathBuf::from("/tmp/evo-concurrency-test-data-root"),
        None,
        PluginsSecurityConfig::default(),
    );

    // Two slow respondents share the same OverlapTracker so the test
    // can correlate their start/finish timestamps after the fact.
    let tracker = OverlapTracker::new();

    let plugin_a = SlowRespondent {
        name: "org.test.slow.a".into(),
        which: 'a',
        tracker: Arc::clone(&tracker),
        loaded: false,
    };
    let plugin_b = SlowRespondent {
        name: "org.test.slow.b".into(),
        which: 'b',
        tracker: Arc::clone(&tracker),
        loaded: false,
    };

    engine
        .admit_singleton_respondent(
            plugin_a,
            slow_manifest("org.test.slow.a", "a"),
        )
        .await
        .expect("admit slow respondent a");
    engine
        .admit_singleton_respondent(
            plugin_b,
            slow_manifest("org.test.slow.b", "b"),
        )
        .await
        .expect("admit slow respondent b");

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

    // Two independent client connections. We use distinct connections
    // (rather than two requests on one connection) because the
    // server's frame loop processes a single connection sequentially:
    // proving concurrency across plugins requires concurrent dispatch
    // paths, which the connection-per-request fan-out provides.
    let mut stream_a = UnixStream::connect(&socket_path)
        .await
        .expect("connect client a");
    let mut stream_b = UnixStream::connect(&socket_path)
        .await
        .expect("connect client b");

    let req_a = r#"{"op":"request","shelf":"parallel.a","request_type":"slow","payload_b64":""}"#;
    let req_b = r#"{"op":"request","shelf":"parallel.b","request_type":"slow","payload_b64":""}"#;

    let wall_start = Instant::now();
    let (resp_a, resp_b) = tokio::join!(
        async {
            write_frame(&mut stream_a, req_a.as_bytes()).await;
            read_frame(&mut stream_a).await
        },
        async {
            write_frame(&mut stream_b, req_b.as_bytes()).await;
            read_frame(&mut stream_b).await
        }
    );
    let wall_elapsed = wall_start.elapsed();

    // Both responses must indicate success (the slow respondents
    // echo their empty payload back).
    let v_a: serde_json::Value =
        serde_json::from_slice(&resp_a).expect("response a JSON");
    let v_b: serde_json::Value =
        serde_json::from_slice(&resp_b).expect("response b JSON");
    let payload_a_b64 = v_a
        .get("payload_b64")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("response a missing payload_b64: {v_a}"));
    let payload_b_b64 = v_b
        .get("payload_b64")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("response b missing payload_b64: {v_b}"));
    assert_eq!(B64.decode(payload_a_b64).expect("decode a"), b"");
    assert_eq!(B64.decode(payload_b_b64).expect("decode b"), b"");

    // Read out the recorded start/finish timestamps. Both pairs must
    // be populated by the time the responses arrive.
    let a_started = tracker.a_started_us.load(Ordering::SeqCst);
    let a_finished = tracker.a_finished_us.load(Ordering::SeqCst);
    let b_started = tracker.b_started_us.load(Ordering::SeqCst);
    let b_finished = tracker.b_finished_us.load(Ordering::SeqCst);
    assert!(a_started >= 0, "respondent a never recorded its start");
    assert!(a_finished >= 0, "respondent a never recorded its finish");
    assert!(b_started >= 0, "respondent b never recorded its start");
    assert!(b_finished >= 0, "respondent b never recorded its finish");

    // Overlap proof: the second-to-start handler must have begun
    // strictly before the first-to-start handler finished. If
    // dispatch were serialised on the engine mutex, the second start
    // could not occur until after the first finish.
    let (first_start, first_finish, second_start) = if a_started <= b_started {
        (a_started, a_finished, b_started)
    } else {
        (b_started, b_finished, a_started)
    };
    assert!(
        second_start < first_finish,
        "handlers did not overlap: \
         first_start={first_start}us, first_finish={first_finish}us, \
         second_start={second_start}us; \
         this means dispatch is still serialising on a single mutex"
    );

    // Sanity: wall time should be substantially less than the
    // sequential lower bound (2 * 100ms = 200ms). 180ms leaves ample
    // slack for scheduler jitter while still being well below the
    // sequential floor. This is a weaker check than the overlap
    // assertion above but catches regressions where the handlers
    // overlap only fractionally.
    assert!(
        wall_elapsed < Duration::from_millis(180),
        "two concurrent slow requests took {wall_elapsed:?}; \
         expected well under 200ms (the sequential lower bound). \
         Handlers may be queueing on a serialised dispatch path."
    );

    drop(stream_a);
    drop(stream_b);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task join");

    engine.shutdown().await.expect("drain admission engine");
}

// ---------------------------------------------------------------------
// Test helpers (local copies; kept self-contained to avoid coupling
// this proof test to the larger end-to-end harness).
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
