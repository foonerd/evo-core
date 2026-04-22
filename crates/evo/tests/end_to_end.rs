//! End-to-end integration test for the steward skeleton.
//!
//! Starts a steward in the same process, connects a client to its Unix
//! socket, sends an echo request, and verifies the roundtrip. Proves the
//! config -> catalogue -> admission -> server -> plugin -> response chain
//! works end-to-end.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::server::Server;

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

#[tokio::test]
async fn echo_roundtrip_through_socket() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let socket_path = tmp.path().join("evo.sock");
    let catalogue_path = tmp.path().join("catalogue.toml");
    std::fs::write(&catalogue_path, CATALOGUE_TOML).expect("write catalogue");

    // Load catalogue, admit the echo plugin.
    let catalogue = Catalogue::load(&catalogue_path).expect("catalogue");
    let mut engine = AdmissionEngine::new();
    let echo_plugin = evo_example_echo::EchoPlugin::new();
    let echo_manifest = evo_example_echo::manifest();
    engine
        .admit_singleton_respondent(echo_plugin, echo_manifest, &catalogue)
        .await
        .expect("admit echo plugin");

    let engine = Arc::new(Mutex::new(engine));
    let server = Server::new(socket_path.clone(), Arc::clone(&engine));

    // Shutdown channel. The test drives server shutdown explicitly at the
    // end so the connection-handler cleanup is deterministic.
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

    // Wait for the server to bind its socket. A retry loop is more
    // robust than a fixed sleep on busy CI.
    wait_for_socket(&socket_path, Duration::from_secs(2))
        .await
        .expect("server socket became available");

    // Connect as a client.
    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect to steward socket");

    // Send an echo request.
    let payload = b"hello, evo";
    let request_json = format!(
        r#"{{"shelf":"example.echo","request_type":"echo","payload_b64":"{}"}}"#,
        B64.encode(payload)
    );
    let request_bytes = request_json.as_bytes();
    let request_len = (request_bytes.len() as u32).to_be_bytes();
    stream.write_all(&request_len).await.expect("write len");
    stream.write_all(request_bytes).await.expect("write body");
    stream.flush().await.expect("flush");

    // Read the response.
    let mut response_len_buf = [0u8; 4];
    stream
        .read_exact(&mut response_len_buf)
        .await
        .expect("read response len");
    let response_len = u32::from_be_bytes(response_len_buf) as usize;
    assert!(response_len > 0, "response length must be non-zero");
    assert!(
        response_len < 64 * 1024,
        "response length suspiciously large: {response_len}"
    );

    let mut response_body = vec![0u8; response_len];
    stream
        .read_exact(&mut response_body)
        .await
        .expect("read response body");

    // Parse the response and verify the echoed payload.
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

    // Close the client side of the connection cleanly.
    drop(stream);

    // Signal shutdown and wait for the server task to complete.
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task join");

    // Drain the engine. Verifies unload runs without error.
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
    let catalogue_path = tmp.path().join("catalogue.toml");
    std::fs::write(&catalogue_path, CATALOGUE_TOML).expect("write catalogue");

    let catalogue = Catalogue::load(&catalogue_path).expect("catalogue");
    let mut engine = AdmissionEngine::new();
    let echo_plugin = evo_example_echo::EchoPlugin::new();
    let echo_manifest = evo_example_echo::manifest();
    engine
        .admit_singleton_respondent(echo_plugin, echo_manifest, &catalogue)
        .await
        .expect("admit echo");

    let engine = Arc::new(Mutex::new(engine));
    let server = Server::new(socket_path.clone(), Arc::clone(&engine));

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

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect");

    // Address a shelf that does not exist.
    let request_json =
        r#"{"shelf":"does.not.exist","request_type":"echo","payload_b64":""}"#;
    let request_len = (request_json.len() as u32).to_be_bytes();
    stream.write_all(&request_len).await.expect("write len");
    stream.write_all(request_json.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");

    let mut response_len_buf = [0u8; 4];
    stream
        .read_exact(&mut response_len_buf)
        .await
        .expect("response len");
    let response_len = u32::from_be_bytes(response_len_buf) as usize;

    let mut response_body = vec![0u8; response_len];
    stream
        .read_exact(&mut response_body)
        .await
        .expect("response body");

    let response_value: serde_json::Value =
        serde_json::from_slice(&response_body).expect("JSON");
    assert!(
        response_value.get("error").and_then(|v| v.as_str()).is_some(),
        "expected error field in response, got: {}",
        String::from_utf8_lossy(&response_body)
    );

    drop(stream);
    let _ = shutdown_tx.send(());
    server_task.await.expect("server task");

    engine.lock().await.shutdown().await.expect("drain");
}

/// Wait up to `timeout` for a Unix socket file to appear and accept
/// connections.
async fn wait_for_socket(
    path: &std::path::Path,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            if UnixStream::connect(path).await.is_ok() {
                return Ok(());
            }
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
