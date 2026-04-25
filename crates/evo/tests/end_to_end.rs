//! End-to-end integration tests for the steward skeleton.
//!
//! Start a steward in the same process, connect a client to its Unix
//! socket, send requests, verify the responses. Proves the config ->
//! catalogue -> admission -> server -> plugin -> response and
//! config -> catalogue -> admission -> server -> projection-engine ->
//! registry chains work end-to-end.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::happenings::{Happening, HappeningBus};
use evo::projections::ProjectionEngine;
use evo::relations::RelationGraph;
use evo::server::Server;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::{
    CustodyHandle, ExternalAddressing, HealthStatus, SubjectAnnouncement,
};

const CATALOGUE_TOML: &str = r#"
[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Example rack for the v0 skeleton test."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echoes inputs back."
"#;

/// Build a steward harness: admission engine with the echo plugin
/// admitted, a projection engine over its stores, and a ready-to-run
/// server. The caller drives the server and is responsible for
/// teardown.
async fn build_harness(
    socket_path: std::path::PathBuf,
    catalogue_toml: &str,
) -> (Arc<Mutex<AdmissionEngine>>, Arc<ProjectionEngine>, Server) {
    let tmp_parent = socket_path.parent().unwrap().to_path_buf();
    let catalogue_path = tmp_parent.join("catalogue.toml");
    std::fs::write(&catalogue_path, catalogue_toml).expect("write catalogue");
    // Wrap in Arc: the catalogue is held by the steward state bag and
    // by every admitted plugin's RegistryRelationAnnouncer.
    let catalogue =
        Arc::new(Catalogue::load(&catalogue_path).expect("catalogue"));

    let state = StewardState::builder()
        .catalogue(catalogue)
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .build()
        .expect("steward state must build");

    let mut engine = AdmissionEngine::new(
        Arc::clone(&state),
        std::path::PathBuf::from("/tmp/evo-end-to-end-tests-data-root"),
        None,
        PluginsSecurityConfig::default(),
    );
    let echo_plugin = evo_example_echo::EchoPlugin::new();
    let echo_manifest = evo_example_echo::manifest();
    engine
        .admit_singleton_respondent(echo_plugin, echo_manifest)
        .await
        .expect("admit echo plugin");

    let projections = Arc::new(ProjectionEngine::new(
        Arc::clone(&state.subjects),
        Arc::clone(&state.relations),
    ));
    let engine = Arc::new(Mutex::new(engine));
    let server = Server::new(
        socket_path,
        Arc::clone(&engine),
        Arc::clone(&state),
        Arc::clone(&projections),
    );

    (engine, projections, server)
}

#[tokio::test]
async fn echo_roundtrip_through_socket() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_socket = socket_path.clone();
    let server_task = tokio::spawn(async move {
        server
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("server run");
        drop(server_socket);
    });

    wait_for_socket(&socket_path, Duration::from_secs(2))
        .await
        .expect("server socket became available");

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect to steward socket");

    // Echo request in the new tagged shape.
    let payload = b"hello, evo";
    let request_json = format!(
        r#"{{"op":"request","shelf":"example.echo","request_type":"echo","payload_b64":"{}"}}"#,
        B64.encode(payload)
    );
    write_frame(&mut stream, request_json.as_bytes()).await;

    let response_body = read_frame(&mut stream).await;
    let response_value: serde_json::Value =
        serde_json::from_slice(&response_body).expect("response JSON");
    let returned_b64 = response_value
        .get("payload_b64")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!(
                "response did not contain payload_b64: {}",
                String::from_utf8_lossy(&response_body)
            )
        });
    let returned_payload = B64.decode(returned_b64).expect("base64 decode");
    assert_eq!(&returned_payload, payload);

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task join");

    engine
        .lock()
        .await
        .shutdown()
        .await
        .expect("drain admission engine");
}

#[tokio::test]
async fn unknown_shelf_returns_structured_error() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

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
        .expect("socket available");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    let request_json = r#"{"op":"request","shelf":"does.not.exist","request_type":"echo","payload_b64":""}"#;
    write_frame(&mut stream, request_json.as_bytes()).await;

    let response_body = read_frame(&mut stream).await;
    let response_value: serde_json::Value =
        serde_json::from_slice(&response_body).expect("JSON");
    assert!(
        response_value
            .get("error")
            .and_then(|v| v.as_str())
            .is_some(),
        "expected error field in response, got: {}",
        String::from_utf8_lossy(&response_body)
    );

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");

    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn project_subject_roundtrips_through_socket() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

    // Pre-populate a subject and a relation via the engine's shared
    // registry and graph, standing in for a plugin contribution. This
    // exercises the read path without requiring a plugin that
    // announces subjects (the echo plugin does not).
    let (track_id, album_id) = {
        let guard = engine.lock().await;
        let registry = guard.registry();
        let graph = guard.relation_graph();

        let track_ann = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("mpd-path", "/x.flac")],
        );
        let track_outcome =
            registry.announce(&track_ann, "com.test.fixture").unwrap();
        let track_id = match track_outcome {
            evo::subjects::AnnounceOutcome::Created(id) => id,
            other => panic!("expected Created, got {other:?}"),
        };

        let album_ann = SubjectAnnouncement::new(
            "album",
            vec![ExternalAddressing::new("mbid", "album-123")],
        );
        let album_outcome =
            registry.announce(&album_ann, "com.test.fixture").unwrap();
        let album_id = match album_outcome {
            evo::subjects::AnnounceOutcome::Created(id) => id,
            other => panic!("expected Created, got {other:?}"),
        };

        graph
            .assert(&track_id, "album_of", &album_id, "com.test.fixture", None)
            .unwrap();

        (track_id, album_id)
    };

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
        .expect("socket available");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    // Minimal project request: no scope (no relation traversal).
    let minimal_req =
        format!(r#"{{"op":"project_subject","canonical_id":"{track_id}"}}"#);
    write_frame(&mut stream, minimal_req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(v["canonical_id"].as_str(), Some(track_id.as_str()));
    assert_eq!(v["subject_type"].as_str(), Some("track"));
    assert_eq!(v["shape_version"].as_u64(), Some(1));
    assert_eq!(v["degraded"].as_bool(), Some(false));
    assert_eq!(v["addressings"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(v["addressings"][0]["scheme"].as_str(), Some("mpd-path"));
    // No scope means no related subjects even though album_of exists.
    assert_eq!(v["related"].as_array().map(|a| a.len()), Some(0));

    // Scoped project request: include album_of forward.
    let scoped_req = format!(
        r#"{{
            "op": "project_subject",
            "canonical_id": "{track_id}",
            "scope": {{
                "relation_predicates": ["album_of"],
                "direction": "forward"
            }}
        }}"#
    );
    write_frame(&mut stream, scoped_req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let related = v["related"].as_array().expect("related array");
    assert_eq!(related.len(), 1);
    assert_eq!(related[0]["predicate"].as_str(), Some("album_of"));
    assert_eq!(related[0]["direction"].as_str(), Some("forward"));
    assert_eq!(related[0]["target_id"].as_str(), Some(album_id.as_str()));
    assert_eq!(related[0]["target_type"].as_str(), Some("album"));

    // Unknown subject yields an error response.
    let bad_req = r#"{"op":"project_subject","canonical_id":"not-a-real-id"}"#;
    write_frame(&mut stream, bad_req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert!(
        v["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown subject"),
        "expected unknown subject error, got: {}",
        String::from_utf8_lossy(&body)
    );

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");

    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn project_subject_multi_hop_roundtrips_through_socket() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

    // Build a 3-chain: track -> album -> artist.
    let (track_id, album_id, artist_id) = {
        let guard = engine.lock().await;
        let registry = guard.registry();
        let graph = guard.relation_graph();

        let track_ann = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("mpd-path", "/x.flac")],
        );
        let album_ann = SubjectAnnouncement::new(
            "album",
            vec![ExternalAddressing::new("mbid", "album-123")],
        );
        let artist_ann = SubjectAnnouncement::new(
            "artist",
            vec![ExternalAddressing::new("mbid", "artist-456")],
        );

        let t = match registry.announce(&track_ann, "com.test.fixture").unwrap()
        {
            evo::subjects::AnnounceOutcome::Created(id) => id,
            other => panic!("expected Created for track, got {other:?}"),
        };
        let a = match registry.announce(&album_ann, "com.test.fixture").unwrap()
        {
            evo::subjects::AnnounceOutcome::Created(id) => id,
            other => panic!("expected Created for album, got {other:?}"),
        };
        let r =
            match registry.announce(&artist_ann, "com.test.fixture").unwrap() {
                evo::subjects::AnnounceOutcome::Created(id) => id,
                other => panic!("expected Created for artist, got {other:?}"),
            };

        graph
            .assert(&t, "rel", &a, "com.test.fixture", None)
            .unwrap();
        graph
            .assert(&a, "rel", &r, "com.test.fixture", None)
            .unwrap();

        (t, a, r)
    };

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
        .expect("socket available");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    // Request with max_depth=3: expect track -> album (nested) ->
    // artist (nested, leaf).
    let req = format!(
        r#"{{
            "op": "project_subject",
            "canonical_id": "{track_id}",
            "scope": {{
                "relation_predicates": ["rel"],
                "direction": "forward",
                "max_depth": 3
            }}
        }}"#
    );
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");

    // Root: track.
    assert_eq!(v["canonical_id"].as_str(), Some(track_id.as_str()));
    assert_eq!(v["subject_type"].as_str(), Some("track"));
    assert_eq!(v["walk_truncated"].as_bool(), Some(false));

    // Level 1: album, nested.
    let album_rel = &v["related"][0];
    assert_eq!(album_rel["target_id"].as_str(), Some(album_id.as_str()));
    assert_eq!(album_rel["target_type"].as_str(), Some("album"));
    let album_nested = &album_rel["nested"];
    assert!(
        !album_nested.is_null(),
        "album should carry nested projection at depth=3"
    );
    assert_eq!(
        album_nested["canonical_id"].as_str(),
        Some(album_id.as_str())
    );

    // Level 2: artist, nested, leaf (no further edges).
    let artist_rel = &album_nested["related"][0];
    assert_eq!(artist_rel["target_id"].as_str(), Some(artist_id.as_str()));
    assert_eq!(artist_rel["target_type"].as_str(), Some("artist"));
    let artist_nested = &artist_rel["nested"];
    assert!(
        !artist_nested.is_null(),
        "artist should carry nested projection at depth=3"
    );
    assert_eq!(
        artist_nested["related"].as_array().map(|a| a.len()),
        Some(0),
        "artist is a leaf: no outgoing relations"
    );

    // Follow-up request on the same connection: depth=1 should
    // emit a reference without nesting (no recursive expansion).
    let shallow_req = format!(
        r#"{{
            "op": "project_subject",
            "canonical_id": "{track_id}",
            "scope": {{
                "relation_predicates": ["rel"],
                "direction": "forward",
                "max_depth": 1
            }}
        }}"#
    );
    write_frame(&mut stream, shallow_req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let shallow_rel = &v["related"][0];
    assert_eq!(shallow_rel["target_id"].as_str(), Some(album_id.as_str()));
    assert!(
        shallow_rel["nested"].is_null(),
        "depth=1 should not nest, got: {}",
        shallow_rel["nested"]
    );

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");

    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn invalid_op_returns_structured_error() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

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
        .expect("socket available");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    let bad_req = r#"{"op":"who_knows","x":"y"}"#;
    write_frame(&mut stream, bad_req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert!(
        v["error"].as_str().is_some(),
        "expected error response, got: {}",
        String::from_utf8_lossy(&body)
    );

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");

    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn list_active_custodies_empty_when_none_taken() {
    // End-to-end exercise of the list_active_custodies socket
    // surface with an empty ledger. Verifies the op parses, the
    // handler runs, and the response shape is
    // `{"active_custodies": []}`.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

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
        .expect("socket available");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    let req = r#"{"op":"list_active_custodies"}"#;
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let arr = v["active_custodies"].as_array().unwrap_or_else(|| {
        panic!(
            "expected active_custodies array, got: {}",
            String::from_utf8_lossy(&body)
        )
    });
    assert_eq!(arr.len(), 0);

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn list_active_custodies_returns_populated_ledger() {
    // Pre-populate the ledger from the test side (bypassing the
    // take_custody path, which is covered by admission tests and
    // wire_client tests), then query via the socket. Proves the
    // list_active_custodies surface end-to-end: record -> ledger
    // -> wire serialisation -> client JSON.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

    // Populate the ledger directly. Release the engine lock before
    // spawning the server so the server task is free to acquire it
    // during handle_list_active_custodies.
    {
        let guard = engine.lock().await;
        let ledger = guard.custody_ledger();
        ledger.record_custody(
            "org.test.warden",
            "example.custody",
            &CustodyHandle::new("custody-1"),
            "playback",
        );
        ledger.record_state(
            "org.test.warden",
            "custody-1",
            b"state=playing".to_vec(),
            HealthStatus::Healthy,
        );
    }

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
        .expect("socket available");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    let req = r#"{"op":"list_active_custodies"}"#;
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let arr = v["active_custodies"].as_array().unwrap_or_else(|| {
        panic!(
            "expected active_custodies array, got: {}",
            String::from_utf8_lossy(&body)
        )
    });
    assert_eq!(arr.len(), 1);

    let first = &arr[0];
    assert_eq!(first["plugin"].as_str(), Some("org.test.warden"));
    assert_eq!(first["handle_id"].as_str(), Some("custody-1"));
    assert_eq!(first["shelf"].as_str(), Some("example.custody"));
    assert_eq!(first["custody_type"].as_str(), Some("playback"));
    assert_eq!(first["last_state"]["health"].as_str(), Some("healthy"));
    let decoded = B64
        .decode(
            first["last_state"]["payload_b64"]
                .as_str()
                .expect("payload_b64 string"),
        )
        .expect("base64 decode");
    assert_eq!(decoded, b"state=playing");

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn subscribe_happenings_delivers_ack_and_events() {
    // End-to-end exercise of the subscribe_happenings socket
    // surface. Subscribe, read the ack, emit happenings on the
    // bus from the test side,
    // verify the happening frames arrive correctly. Uses direct bus
    // emission rather than take_custody/release_custody because the
    // engine's custody verbs require a warden admitted on the shelf;
    // the subscription flow is independent of which source emits to
    // the bus, so a direct emit is a cleaner test fixture.
    //
    // The ack is load-bearing for this test: because the server's
    // bus.subscribe() runs before the ack is written, once the client
    // has read the ack any subsequent emit on the bus is guaranteed
    // to reach the subscriber. Without the ack the test would be
    // timing-coupled (emit might land before subscribe registers).
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

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
        .expect("socket available");

    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

    // Grab a handle to the bus so we can emit from the test side.
    // Taken BEFORE sending the subscribe op so the Arc clone is
    // ready to use as soon as the ack is read.
    let bus: std::sync::Arc<HappeningBus> = {
        let guard = engine.lock().await;
        guard.happening_bus()
    };

    // Send subscribe_happenings.
    write_frame(&mut stream, br#"{"op":"subscribe_happenings"}"#).await;

    // Read the ack. The ack is written AFTER the server's
    // bus.subscribe() call, so any emit after this point reaches the
    // subscriber.
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        v["subscribed"].as_bool(),
        Some(true),
        "expected subscribed ack, got: {}",
        String::from_utf8_lossy(&body)
    );

    // Emit CustodyTaken on the bus.
    bus.emit(Happening::CustodyTaken {
        plugin: "org.test.warden".into(),
        handle_id: "c-1".into(),
        shelf: "example.custody".into(),
        custody_type: "playback".into(),
        at: std::time::SystemTime::now(),
    });

    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        v["happening"]["type"].as_str(),
        Some("custody_taken"),
        "got: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(v["happening"]["plugin"].as_str(), Some("org.test.warden"));
    assert_eq!(v["happening"]["handle_id"].as_str(), Some("c-1"));
    assert_eq!(v["happening"]["shelf"].as_str(), Some("example.custody"));
    assert_eq!(v["happening"]["custody_type"].as_str(), Some("playback"));
    assert!(
        v["happening"]["at_ms"].as_u64().is_some(),
        "at_ms must be present"
    );

    // Emit a second happening of a different variant to verify the
    // stream stays open and delivers subsequent events.
    bus.emit(Happening::CustodyReleased {
        plugin: "org.test.warden".into(),
        handle_id: "c-1".into(),
        at: std::time::SystemTime::now(),
    });

    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(v["happening"]["type"].as_str(), Some("custody_released"));
    assert_eq!(v["happening"]["handle_id"].as_str(), Some("c-1"));

    // Client disconnects; subscription task exits cleanly.
    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

// ---------------------------------------------------------------------
// project_subject + describe_alias: alias-aware client API surface.
//
// Each test below builds a fresh harness, mutates the registry to
// produce a known alias topology (merge or split), and exercises the
// client-socket op surface end-to-end. The shared
// `setup_alias_harness` helper owns the boilerplate around server
// startup, socket connection, and shutdown so each test reads as a
// linear story of "produce alias state -> hit op -> assert shape".
// ---------------------------------------------------------------------

/// Common setup for alias-aware client-API tests. Returns a harness
/// the test mutates (registry, optional graph) plus a connected
/// client stream and shutdown plumbing. Caller arranges alias state
/// via the returned engine, then issues ops over the stream.
async fn setup_alias_harness() -> (
    Arc<Mutex<AdmissionEngine>>,
    UnixStream,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");

    let (engine, _projections, server) =
        build_harness(socket_path.clone(), CATALOGUE_TOML).await;

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
        .expect("socket available");

    let stream = UnixStream::connect(&socket_path).await.expect("connect");

    (engine, stream, shutdown_tx, server_task, tmp)
}

/// Announce two subjects of the same type and merge them via the
/// registry's `merge_aliases` storage primitive. Returns the two
/// retired source IDs and the new terminal ID. The engine lock is
/// released before running merge_aliases so the registry's internal
/// mutex stays uncontended with the server task.
async fn merge_two_subjects(
    engine: &Arc<Mutex<AdmissionEngine>>,
) -> (String, String, String) {
    let registry = {
        let guard = engine.lock().await;
        guard.registry()
    };

    let a = SubjectAnnouncement::new(
        "track",
        vec![ExternalAddressing::new("mpd-path", "/a.flac")],
    );
    let b = SubjectAnnouncement::new(
        "track",
        vec![ExternalAddressing::new("mpd-path", "/b.flac")],
    );
    let id_a = match registry.announce(&a, "com.test.fixture").unwrap() {
        evo::subjects::AnnounceOutcome::Created(id) => id,
        other => panic!("expected Created for a, got {other:?}"),
    };
    let id_b = match registry.announce(&b, "com.test.fixture").unwrap() {
        evo::subjects::AnnounceOutcome::Created(id) => id,
        other => panic!("expected Created for b, got {other:?}"),
    };

    let outcome = registry
        .merge_aliases(&id_a, &id_b, "admin.plugin", Some("dedup".into()))
        .expect("merge_aliases");
    let new_id = outcome.new_id;

    (id_a, id_b, new_id)
}

/// Announce one subject with two addressings and split it across
/// the partition. Returns the retired source ID and the two new
/// canonical IDs.
async fn split_one_subject(
    engine: &Arc<Mutex<AdmissionEngine>>,
) -> (String, Vec<String>) {
    let registry = {
        let guard = engine.lock().await;
        guard.registry()
    };

    let subj = SubjectAnnouncement::new(
        "track",
        vec![
            ExternalAddressing::new("mpd-path", "/x.flac"),
            ExternalAddressing::new("mpd-path", "/y.flac"),
        ],
    );
    let source_id = match registry.announce(&subj, "com.test.fixture").unwrap()
    {
        evo::subjects::AnnounceOutcome::Created(id) => id,
        other => panic!("expected Created for split source, got {other:?}"),
    };

    let outcome = registry
        .split_subject(
            &source_id,
            vec![
                vec![ExternalAddressing::new("mpd-path", "/x.flac")],
                vec![ExternalAddressing::new("mpd-path", "/y.flac")],
            ],
            "admin.plugin",
            Some("audit".into()),
        )
        .expect("split_subject");

    (source_id, outcome.new_ids)
}

#[tokio::test]
async fn project_subject_with_aliased_from_when_id_was_merged() {
    let (engine, mut stream, shutdown_tx, server_task, _tmp) =
        setup_alias_harness().await;
    let (id_a, _id_b, new_id) = merge_two_subjects(&engine).await;

    // Query the merged-away ID. Default follow_aliases:true means
    // the steward auto-follows to the terminal and projects it.
    let req = format!(r#"{{"op":"project_subject","canonical_id":"{id_a}"}}"#);
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");

    // subject is populated and is the terminal projection.
    assert!(
        !v["subject"].is_null(),
        "subject must be populated, got: {v}"
    );
    assert_eq!(v["subject"]["canonical_id"].as_str(), Some(new_id.as_str()));
    assert_eq!(v["subject"]["subject_type"].as_str(), Some("track"));

    // aliased_from carries the chain (length 1: single merge hop)
    // and the terminal_id.
    let af = &v["aliased_from"];
    assert_eq!(af["queried_id"].as_str(), Some(id_a.as_str()));
    assert_eq!(af["terminal_id"].as_str(), Some(new_id.as_str()));
    let chain = af["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 1, "chain should be length 1, got: {chain:?}");
    assert_eq!(chain[0]["kind"].as_str(), Some("merged"));
    assert_eq!(chain[0]["old_id"].as_str(), Some(id_a.as_str()));

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn project_subject_with_aliased_from_when_id_was_split() {
    let (engine, mut stream, shutdown_tx, server_task, _tmp) =
        setup_alias_harness().await;
    let (source_id, new_ids) = split_one_subject(&engine).await;

    let req =
        format!(r#"{{"op":"project_subject","canonical_id":"{source_id}"}}"#);
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");

    // subject is null because the chain forked.
    assert!(
        v["subject"].is_null(),
        "subject must be null on split fork, got: {v}"
    );

    // aliased_from has the split record with terminal_id null.
    let af = &v["aliased_from"];
    assert_eq!(af["queried_id"].as_str(), Some(source_id.as_str()));
    assert!(
        af["terminal_id"].is_null(),
        "terminal_id must be null on a split fork, got: {af}"
    );
    let chain = af["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0]["kind"].as_str(), Some("split"));
    let chain_new_ids: Vec<&str> = chain[0]["new_ids"]
        .as_array()
        .expect("new_ids array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for new_id in &new_ids {
        assert!(
            chain_new_ids.contains(&new_id.as_str()),
            "new_id {new_id} should appear in chain[0].new_ids"
        );
    }

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn project_subject_returns_not_found_for_unknown_id_no_alias() {
    let (engine, mut stream, shutdown_tx, server_task, _tmp) =
        setup_alias_harness().await;

    let req = r#"{"op":"project_subject","canonical_id":"never-existed"}"#;
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");

    // Existing NotFound shape: error string, no aliased_from key.
    assert!(
        v["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown subject"),
        "expected unknown subject error, got: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        v.get("aliased_from").is_none(),
        "aliased_from must be absent when the queried ID is genuinely \
         unknown, got: {v}"
    );

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn project_subject_with_follow_aliases_false_does_not_auto_follow() {
    let (engine, mut stream, shutdown_tx, server_task, _tmp) =
        setup_alias_harness().await;
    let (id_a, _id_b, new_id) = merge_two_subjects(&engine).await;

    // Same merge as the auto-follow test, but the request opts out.
    let req = format!(
        r#"{{"op":"project_subject","canonical_id":"{id_a}","follow_aliases":false}}"#
    );
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");

    assert!(
        v["subject"].is_null(),
        "subject must be null when follow_aliases is false, got: {v}"
    );
    let af = &v["aliased_from"];
    assert_eq!(af["queried_id"].as_str(), Some(id_a.as_str()));
    // terminal_id is still populated: opting out of auto-follow does
    // not erase the steward's knowledge of where the chain ends, so
    // the consumer can issue a follow-up project against new_id if
    // they choose.
    assert_eq!(af["terminal_id"].as_str(), Some(new_id.as_str()));

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn describe_alias_returns_full_chain_with_include_chain_true_default() {
    let (engine, mut stream, shutdown_tx, server_task, _tmp) =
        setup_alias_harness().await;
    let (id_a, _id_b, new_id) = merge_two_subjects(&engine).await;

    // No include_chain field: default-true walks the chain.
    let req = format!(r#"{{"op":"describe_alias","subject_id":"{id_a}"}}"#);
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");

    assert_eq!(v["ok"].as_bool(), Some(true));
    assert_eq!(v["subject_id"].as_str(), Some(id_a.as_str()));
    assert_eq!(v["result"]["kind"].as_str(), Some("aliased"));
    let chain = v["result"]["chain"].as_array().expect("chain");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0]["kind"].as_str(), Some("merged"));
    // Terminal is populated because the chain resolves to a live
    // subject.
    assert!(!v["result"]["terminal"].is_null());
    assert_eq!(
        v["result"]["terminal"]["id"].as_str(),
        Some(new_id.as_str())
    );

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn describe_alias_returns_immediate_record_with_include_chain_false() {
    let (engine, mut stream, shutdown_tx, server_task, _tmp) =
        setup_alias_harness().await;
    let (id_a, _id_b, _new_id) = merge_two_subjects(&engine).await;

    let req = format!(
        r#"{{"op":"describe_alias","subject_id":"{id_a}","include_chain":false}}"#
    );
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");

    assert_eq!(v["ok"].as_bool(), Some(true));
    assert_eq!(v["result"]["kind"].as_str(), Some("aliased"));
    let chain = v["result"]["chain"].as_array().expect("chain");
    assert_eq!(
        chain.len(),
        1,
        "single-hop view must carry exactly one record, got: {chain:?}"
    );
    // Single-hop view never carries a terminal; the caller decides
    // whether to chase the new_ids themselves.
    assert!(
        v["result"]["terminal"].is_null(),
        "single-hop must not project a terminal, got: {v}"
    );

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

#[tokio::test]
async fn describe_alias_returns_not_found_for_unknown_id() {
    let (engine, mut stream, shutdown_tx, server_task, _tmp) =
        setup_alias_harness().await;

    let req = r#"{"op":"describe_alias","subject_id":"never-existed"}"#;
    write_frame(&mut stream, req.as_bytes()).await;
    let body = read_frame(&mut stream).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");

    assert_eq!(v["ok"].as_bool(), Some(true));
    assert_eq!(v["result"]["kind"].as_str(), Some("not_found"));

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");
    engine.lock().await.shutdown().await.expect("drain");
}

// ---------------------------------------------------------------------
// Test helpers
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

/// Wait up to `timeout` for a Unix socket file to appear and accept
/// connections.
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
