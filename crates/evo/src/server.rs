//! The client-facing Unix socket server.
//!
//! v0 protocol is deliberately minimal: length-prefixed JSON frames over
//! a Unix domain socket. This is enough to prove the fabric
//! admits-and-dispatches end-to-end. The production protocol (SDK pass
//! 3) is richer and richer-typed; this is the skeleton version.
//!
//! ## Frame format
//!
//! ```text
//! [4-byte big-endian length] [length bytes of UTF-8 JSON]
//! ```
//!
//! ## Request JSON
//!
//! ```json
//! { "shelf": "example.echo",
//!   "request_type": "echo",
//!   "payload_b64": "aGVsbG8=" }
//! ```
//!
//! ## Response JSON
//!
//! Success:
//! ```json
//! { "payload_b64": "aGVsbG8=" }
//! ```
//!
//! Failure:
//! ```json
//! { "error": "no plugin on shelf: foo.bar" }
//! ```

use crate::admission::AdmissionEngine;
use crate::error::StewardError;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use evo_plugin_sdk::contract::Request;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Maximum size of a single JSON frame. Prevents malicious or malformed
/// clients from forcing the steward to allocate unbounded memory.
const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Source of correlation IDs assigned to incoming requests that do not
/// already carry one.
static NEXT_CID: AtomicU64 = AtomicU64::new(1);

/// A client request as it appears on the wire.
#[derive(Debug, Deserialize)]
struct ClientRequest {
    shelf: String,
    request_type: String,
    #[serde(default)]
    payload_b64: String,
}

/// A response as it appears on the wire.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ClientResponse {
    Success { payload_b64: String },
    Error { error: String },
}

/// The Unix socket server.
pub struct Server {
    socket_path: PathBuf,
    engine: Arc<Mutex<AdmissionEngine>>,
}

impl Server {
    /// Construct a server bound to a socket path and sharing an
    /// admission engine. The socket is not created until [`run`] is
    /// called.
    ///
    /// [`run`]: Self::run
    pub fn new(socket_path: PathBuf, engine: Arc<Mutex<AdmissionEngine>>) -> Self {
        Self {
            socket_path,
            engine,
        }
    }

    /// The socket path this server listens on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the accept loop until the shutdown future resolves.
    ///
    /// Creates the socket directory if it does not exist (mode 0755) and
    /// removes any existing socket file at the path before binding.
    ///
    /// Each accepted connection is handled in a dedicated tokio task.
    /// When `shutdown` resolves, the accept loop exits; in-flight
    /// connection tasks are not explicitly joined in v0 (they are
    /// dropped when the tokio runtime winds down).
    pub async fn run<S>(&self, shutdown: S) -> Result<(), StewardError>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        if let Some(parent) = self.socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    if e.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(StewardError::io(
                            format!(
                                "creating socket parent directory {}",
                                parent.display()
                            ),
                            e,
                        ));
                    }
                }
            }
        }

        // Remove any stale socket file left over from a previous run.
        match tokio::fs::remove_file(&self.socket_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(StewardError::io(
                    format!("removing stale socket {}", self.socket_path.display()),
                    e,
                ));
            }
        }

        let listener = UnixListener::bind(&self.socket_path).map_err(|e| {
            StewardError::io(
                format!("binding socket {}", self.socket_path.display()),
                e,
            )
        })?;

        tracing::info!(
            socket = %self.socket_path.display(),
            "server listening"
        );

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let engine = Arc::clone(&self.engine);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, engine).await {
                                    tracing::warn!(
                                        error = %e,
                                        "connection handler failed"
                                    );
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "accept failed"
                            );
                        }
                    }
                }
                _ = &mut shutdown => {
                    tracing::info!("server accept loop exiting");
                    break;
                }
            }
        }

        // Best-effort socket cleanup on exit. If this fails (already
        // removed, permission denied), we log and move on.
        if let Err(e) = tokio::fs::remove_file(&self.socket_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    socket = %self.socket_path.display(),
                    error = %e,
                    "failed to remove socket on shutdown"
                );
            }
        }

        Ok(())
    }
}

/// Handle one accepted client connection.
///
/// Reads frames in a loop until the client closes the connection. Each
/// frame is processed independently; an error handling one frame closes
/// the connection.
async fn handle_connection(
    mut stream: UnixStream,
    engine: Arc<Mutex<AdmissionEngine>>,
) -> Result<(), StewardError> {
    loop {
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(e) => {
                return Err(StewardError::io("reading frame length", e));
            }
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 {
            return Err(StewardError::Dispatch(
                "zero-length frame".to_string(),
            ));
        }
        if len > MAX_FRAME_SIZE {
            return Err(StewardError::Dispatch(format!(
                "frame too large: {len} bytes (max {MAX_FRAME_SIZE})"
            )));
        }

        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await.map_err(|e| {
            StewardError::io("reading frame body", e)
        })?;

        let response = process_frame(&body, &engine).await;

        let response_bytes = serde_json::to_vec(&response).map_err(|e| {
            StewardError::Dispatch(format!("serialising response: {e}"))
        })?;

        if response_bytes.len() > u32::MAX as usize {
            return Err(StewardError::Dispatch(
                "response too large".to_string(),
            ));
        }

        let response_len = (response_bytes.len() as u32).to_be_bytes();
        stream.write_all(&response_len).await.map_err(|e| {
            StewardError::io("writing response length", e)
        })?;
        stream.write_all(&response_bytes).await.map_err(|e| {
            StewardError::io("writing response body", e)
        })?;
        stream.flush().await.map_err(|e| {
            StewardError::io("flushing response", e)
        })?;
    }
}

/// Parse, dispatch, and produce a ClientResponse for one frame.
///
/// Never panics; parse failures and dispatch failures both yield an
/// error response rather than bubbling up to close the connection.
async fn process_frame(
    body: &[u8],
    engine: &Arc<Mutex<AdmissionEngine>>,
) -> ClientResponse {
    let req: ClientRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return ClientResponse::Error {
                error: format!("invalid JSON: {e}"),
            };
        }
    };

    let payload = match B64.decode(&req.payload_b64) {
        Ok(p) => p,
        Err(e) => {
            return ClientResponse::Error {
                error: format!("invalid base64 payload: {e}"),
            };
        }
    };

    let cid = NEXT_CID.fetch_add(1, Ordering::Relaxed);
    let sdk_request = Request {
        request_type: req.request_type,
        payload,
        correlation_id: cid,
        deadline: None,
    };

    let shelf = req.shelf;
    let result = {
        let mut guard = engine.lock().await;
        guard.handle_request(&shelf, sdk_request).await
    };

    match result {
        Ok(resp) => ClientResponse::Success {
            payload_b64: B64.encode(&resp.payload),
        },
        Err(e) => ClientResponse::Error {
            error: format!("{e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;

    #[test]
    fn client_request_parses() {
        let json = r#"{"shelf":"a.b","request_type":"t","payload_b64":"aGVsbG8="}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.shelf, "a.b");
        assert_eq!(r.request_type, "t");
        assert_eq!(B64.decode(r.payload_b64).unwrap(), b"hello");
    }

    #[test]
    fn client_request_payload_defaults_empty() {
        let json = r#"{"shelf":"a.b","request_type":"t"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.payload_b64, "");
    }

    #[test]
    fn client_response_success_serialises() {
        let r = ClientResponse::Success {
            payload_b64: "aGVsbG8=".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("payload_b64"));
        assert!(!s.contains("error"));
    }

    #[test]
    fn client_response_error_serialises() {
        let r = ClientResponse::Error {
            error: "nope".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("error"));
        assert!(!s.contains("payload_b64"));
    }
}
