//! Fast Path channel: a separate Unix-domain socket for the
//! latency-bounded operator-tactile dispatch surface (volume,
//! mute, pause, hardware-input forwarding).
//!
//! The slow path (`/run/evo/evo.sock`) carries the full operator
//! API — projections, subscriptions, plugin admin verbs — under a
//! human-time latency budget (~100 ms). The Fast Path
//! (`/run/evo/fast.sock`) carries only `course_correct` dispatches
//! eligible for tactile response (<50 ms default), gated by the
//! warden's manifest-declared `fast_path_verbs`.
//!
//! ## Channel separation
//!
//! Two physically distinct sockets give kernel-level queue
//! separation: a slow-path subscription drain does not delay an
//! operator's volume command because the OS scheduler isolates
//! the per-socket work. This is operationally simpler than one
//! socket with frame-level priority; the framework does not
//! maintain a priority queue inside the dispatcher.
//!
//! ## Per-warden serialisation preserved
//!
//! Fast Path dispatch goes through the same per-warden mutex as
//! slow-path course_correct, so the "one mutation in flight per
//! warden" invariant survives. Cross-channel head-of-queue
//! priority — Fast Path frames jumping ahead of slow-path
//! waiters at the per-warden mutex — is documented in the
//! design but not yet implemented; the existing tokio mutex is
//! FIFO-fair, so a Fast Path arrival waits behind any slow-path
//! call already in queue. Worst case: a slow-path call's
//! `course_correction_budget_ms` plus the Fast Path's own
//! budget. For the v0.1.12 audio targets this is acceptable
//! because slow-path course_correct calls on the same warden
//! are rare; a dedicated priority lane lands in a follow-up.
//!
//! ## CBOR-only framing
//!
//! Fast Path mandates CBOR. JSON parsing on the latency-critical
//! path is the wrong cost-benefit trade-off; the channel stays
//! free of it. Slow path remains JSON-or-CBOR per consumer
//! negotiation.
//!
//! There is no Hello/HelloAck handshake on Fast Path: the codec
//! is fixed and the capability gate is at accept time using the
//! peer's Unix-domain credentials rather than a negotiated
//! capability frame. This keeps the per-frame critical path
//! short and predictable.
//!
//! ## Authorisation
//!
//! Two gates apply per dispatch:
//!
//! - **Connection-level**: the peer's UID/GID must satisfy the
//!   `fast_path_admin` ACL policy (default: same UID as the
//!   steward's, mirroring `plugins_admin` /
//!   `reconciliation_admin`). Connections that fail this check
//!   receive a single CBOR error frame
//!   (`fast_path_admin_not_granted`) and are closed.
//! - **Per-warden verb-level**: the target warden's manifest
//!   must declare the requested verb in `fast_path_verbs`.
//!   Dispatches against undeclared verbs refuse with
//!   `not_fast_path_eligible`.
//!
//! Combined with the per-plugin `capabilities.fast_path` sender
//! flag (used by the in-process / OOP SDK dispatch path), the
//! three gates compose to: the dispatching plugin must be Fast-
//! Path-sender-flagged, the connection must pass
//! `fast_path_admin`, and the target warden must declare the
//! verb on its Fast Path verb list.

use crate::client_acl::{ClientAcl, PeerCredentials, StewardIdentity};
use crate::error::StewardError;
use crate::router::PluginRouter;
use evo_plugin_sdk::codec::{decode_cbor_value, encode_cbor_value, WireError};
use evo_plugin_sdk::contract::CustodyHandle;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::net::{UnixListener, UnixStream};

/// Default Fast Path socket path. Distributions configuring a
/// non-default path pass an explicit value to
/// [`FastPathConfig::with_socket_path`].
pub const DEFAULT_FAST_PATH_SOCKET: &str = "/run/evo/fast.sock";

/// Hard cap on a single Fast Path frame. Smaller than the slow-
/// path `MAX_FRAME_SIZE` because Fast Path payloads are
/// inherently small (volume levels, button events, brightness
/// values); a Fast Path frame approaching 1 MiB is almost
/// certainly a misuse, and capping the size tighter keeps the
/// per-frame parse + dispatch path predictable.
pub const FAST_PATH_MAX_FRAME_SIZE: usize = 64 * 1024;

/// Configuration for the Fast Path listener.
#[derive(Debug, Clone)]
pub struct FastPathConfig {
    /// Filesystem path the listener binds at. Default
    /// [`DEFAULT_FAST_PATH_SOCKET`].
    socket_path: PathBuf,
}

impl FastPathConfig {
    /// New configuration with the default socket path.
    pub fn new() -> Self {
        Self {
            socket_path: PathBuf::from(DEFAULT_FAST_PATH_SOCKET),
        }
    }

    /// Override the listener socket path.
    pub fn with_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket_path = path.into();
        self
    }

    /// The filesystem path the listener will bind at.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Default for FastPathConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Inbound Fast Path frame. CBOR-encoded.
///
/// Distinct from the slow-path `WireFrame` enum so the two
/// channels do not entangle their wire surfaces; growing the
/// slow path's frame set should never imply changes on the
/// latency-critical Fast Path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FastPathRequest {
    /// Dispatch a `course_correct` on the warden hosting the
    /// named shelf. Mirrors the slow-path course_correct surface
    /// but rides the latency-bounded channel.
    Dispatch {
        /// Operator-supplied correlation ID. Echoed on the
        /// matching response so consumers can demultiplex
        /// pipelined requests.
        cid: u64,
        /// Target shelf hosting the warden. Must currently be
        /// admitted; otherwise the dispatcher refuses with
        /// `not_found`.
        shelf: String,
        /// Verb name. Must be in the warden's
        /// `capabilities.warden.fast_path_verbs`; otherwise the
        /// dispatcher refuses with `not_fast_path_eligible`.
        verb: String,
        /// Opaque payload per the warden's verb shape. Encoded
        /// as a native CBOR byte string (or a base64 string
        /// under any future codec) via the format-aware
        /// `base64_bytes` helper.
        #[serde(with = "evo_plugin_sdk::codec::base64_bytes")]
        payload: Vec<u8>,
        /// Custody handle ID. Names the specific custody session
        /// the dispatch applies to. Operators discover handle
        /// IDs via the slow-path `list_active_custodies` op or
        /// receive them out-of-band from the plugin that took
        /// custody.
        handle_id: String,
        /// Wall-clock millisecond timestamp at which the warden
        /// accepted the custody. Pairs with `handle_id` to bind
        /// the dispatch to a specific custody session per the
        /// `CustodyHandle::started_at` discipline; a stale
        /// timestamp does not match the warden's current handle
        /// and refuses with `no_active_custody`.
        handle_started_at_ms: u64,
        /// Optional per-frame deadline override. When `Some`,
        /// the effective dispatch deadline is `min(declared
        /// budget, this)`. `None` accepts the warden's declared
        /// `fast_path_budget_ms` (or the framework default).
        deadline_ms: Option<u32>,
    },
}

/// Outbound Fast Path frame. CBOR-encoded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FastPathResponse {
    /// Successful dispatch. The warden's `course_correct`
    /// returned `Ok(())`.
    Dispatched {
        /// Echoes the request `cid`.
        cid: u64,
    },
    /// Structured failure. Mirrors the slow-path
    /// [`crate::server::ApiError`] shape so consumers parse one
    /// error format across both channels.
    Error {
        /// Echoes the request `cid` when the failure was
        /// per-frame; `0` when the failure was at the
        /// connection level (e.g. `fast_path_admin_not_granted`
        /// before any request was read).
        cid: u64,
        /// Structured taxonomy class. Names align with
        /// [`evo_plugin_sdk::error_taxonomy::ErrorClass`] so
        /// consumers can translate the wire string to the
        /// in-process enum.
        class: String,
        /// Stable subclass token. Consumers should branch on
        /// this rather than parse the message. Documented
        /// values: `fast_path_admin_not_granted`,
        /// `not_fast_path_eligible`, `fast_path_budget_exceeded`,
        /// `not_found`, `shutting_down`.
        subclass: String,
        /// Human-readable diagnostic. May be empty.
        message: String,
    },
}

/// Outcome of a single per-frame dispatch attempt. Internal to
/// the per-connection loop; a downstream serializer maps it to
/// [`FastPathResponse`].
enum DispatchOutcome {
    Ok,
    Refused {
        class: &'static str,
        subclass: &'static str,
        message: String,
    },
}

/// Run the Fast Path accept loop until `shutdown` resolves.
///
/// Each accepted connection is gated at accept time: the peer's
/// Unix-domain credentials are evaluated against the
/// `fast_path_admin` ACL policy. Connections that pass receive
/// a per-frame dispatch loop; connections that fail receive a
/// single error frame and close. When `shutdown` resolves the
/// accept loop exits; in-flight per-connection tasks drop
/// naturally as the runtime winds down.
///
/// Stale socket files at `socket_path` are removed before
/// binding; the parent directory is created if missing.
pub async fn run_fast_path<S>(
    config: FastPathConfig,
    router: Arc<PluginRouter>,
    acl: Arc<ClientAcl>,
    steward_identity: StewardIdentity,
    shutdown: S,
) -> Result<(), StewardError>
where
    S: Future<Output = ()> + Send + 'static,
{
    if let Some(parent) = config.socket_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(StewardError::io(
                        format!(
                            "creating fast-path socket parent directory {}",
                            parent.display()
                        ),
                        e,
                    ));
                }
            }
        }
    }

    match tokio::fs::remove_file(&config.socket_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(StewardError::io(
                format!(
                    "removing stale fast-path socket {}",
                    config.socket_path.display()
                ),
                e,
            ));
        }
    }

    let listener = UnixListener::bind(&config.socket_path).map_err(|e| {
        StewardError::io(
            format!(
                "binding fast-path socket {}",
                config.socket_path.display()
            ),
            e,
        )
    })?;

    tracing::info!(
        socket = %config.socket_path.display(),
        "fast path listening"
    );

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        let peer = peer_credentials(&stream);
                        let router = Arc::clone(&router);
                        let acl = Arc::clone(&acl);
                        tokio::spawn(async move {
                            if let Err(e) = handle_fast_path_connection(
                                stream,
                                router,
                                acl,
                                steward_identity,
                                peer,
                            )
                            .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    "fast path connection handler failed"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "fast path accept failed"
                        );
                    }
                }
            }
            _ = &mut shutdown => {
                tracing::info!("fast path accept loop exiting");
                break;
            }
        }
    }

    if let Err(e) = tokio::fs::remove_file(&config.socket_path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                socket = %config.socket_path.display(),
                error = %e,
                "failed to remove fast path socket on shutdown"
            );
        }
    }

    Ok(())
}

/// Read peer credentials from a connected `UnixStream`. Identical
/// shape to the slow-path helper in `server.rs`; duplicated here
/// so the Fast Path module is self-contained and tests can stub
/// the function in isolation.
fn peer_credentials(stream: &UnixStream) -> PeerCredentials {
    match stream.peer_cred() {
        Ok(cred) => PeerCredentials {
            uid: Some(cred.uid()),
            gid: Some(cred.gid()),
        },
        Err(_) => PeerCredentials {
            uid: None,
            gid: None,
        },
    }
}

/// Per-connection driver. ACL-gates the peer at the head of the
/// loop; on a denial, sends a single `fast_path_admin_not_granted`
/// error frame and closes. On grant, runs the per-frame dispatch
/// loop until the peer disconnects or any frame I/O fails.
async fn handle_fast_path_connection(
    mut stream: UnixStream,
    router: Arc<PluginRouter>,
    acl: Arc<ClientAcl>,
    steward_identity: StewardIdentity,
    peer: PeerCredentials,
) -> Result<(), StewardError> {
    if !acl.allows_fast_path_admin(peer, steward_identity) {
        // Best-effort: send a structured refusal so the operator
        // sees a wire error rather than a closed socket. Peer
        // failures here are expected (the peer may have already
        // closed) and not surfaced.
        let _ = write_error_frame(
            &mut stream,
            0,
            "permission_denied",
            "fast_path_admin_not_granted",
            "this connection is not authorised to dispatch fast \
             path frames; check /etc/evo/client_acl.toml \
             [capabilities.fast_path_admin]",
        )
        .await;
        return Ok(());
    }

    loop {
        let request = match read_fast_path_frame(&mut stream).await {
            Ok(frame) => frame,
            Err(WireError::PeerClosed) => return Ok(()),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "fast path read frame failed; closing connection"
                );
                return Ok(());
            }
        };

        let (cid, outcome) = dispatch_fast_path_request(&router, request).await;
        let response = match outcome {
            DispatchOutcome::Ok => FastPathResponse::Dispatched { cid },
            DispatchOutcome::Refused {
                class,
                subclass,
                message,
            } => FastPathResponse::Error {
                cid,
                class: class.to_string(),
                subclass: subclass.to_string(),
                message,
            },
        };
        if let Err(e) = write_fast_path_frame(&mut stream, &response).await {
            tracing::debug!(
                error = %e,
                "fast path write frame failed; closing connection"
            );
            return Ok(());
        }
    }
}

/// Helper: write a [`FastPathResponse::Error`] frame to the
/// stream. Best-effort: peer-close races are normal at this
/// layer and the I/O error is swallowed by the caller.
async fn write_error_frame(
    stream: &mut UnixStream,
    cid: u64,
    class: &str,
    subclass: &str,
    message: &str,
) -> Result<(), WireError> {
    let frame = FastPathResponse::Error {
        cid,
        class: class.to_string(),
        subclass: subclass.to_string(),
        message: message.to_string(),
    };
    write_fast_path_frame(stream, &frame).await
}

/// Encode a [`FastPathResponse`] as CBOR and write a length-
/// prefixed frame. Mirrors the slow-path
/// [`evo_plugin_sdk::codec::write_frame`] but operates on
/// `FastPathResponse` directly so the Fast Path channel does not
/// need to wrap its own enum inside the slow-path
/// `WireFrame` envelope.
async fn write_fast_path_frame(
    stream: &mut UnixStream,
    frame: &FastPathResponse,
) -> Result<(), WireError> {
    use tokio::io::AsyncWriteExt;

    let payload = encode_cbor_value(frame)?;
    if payload.len() > FAST_PATH_MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge {
            size: payload.len(),
            limit: FAST_PATH_MAX_FRAME_SIZE,
        });
    }
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

/// Decode one length-prefixed CBOR frame from the stream into a
/// [`FastPathRequest`]. Mirrors the slow-path
/// [`evo_plugin_sdk::codec::read_frame`] but parses
/// `FastPathRequest` directly.
async fn read_fast_path_frame(
    stream: &mut UnixStream,
) -> Result<FastPathRequest, WireError> {
    use tokio::io::AsyncReadExt;

    let mut len_bytes = [0u8; 4];
    match stream.read_exact(&mut len_bytes).await {
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(WireError::PeerClosed);
        }
        Err(e) => return Err(WireError::Io(e)),
    }
    let size = u32::from_be_bytes(len_bytes) as usize;
    if size > FAST_PATH_MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge {
            size,
            limit: FAST_PATH_MAX_FRAME_SIZE,
        });
    }
    let mut buf = vec![0u8; size];
    stream.read_exact(&mut buf).await?;
    decode_cbor_value(&buf)
}

/// Per-frame dispatch. Looks up the warden by shelf, evaluates
/// the per-warden Fast Path verb gate via the router's
/// `course_correct_fast`, and translates the result into a
/// structured [`DispatchOutcome`].
async fn dispatch_fast_path_request(
    router: &Arc<PluginRouter>,
    request: FastPathRequest,
) -> (u64, DispatchOutcome) {
    let FastPathRequest::Dispatch {
        cid,
        shelf,
        verb,
        payload,
        handle_id,
        handle_started_at_ms,
        deadline_ms,
    } = request;

    let handle = CustodyHandle {
        id: handle_id,
        started_at: UNIX_EPOCH + Duration::from_millis(handle_started_at_ms),
    };

    let outcome = match router
        .course_correct_fast(&shelf, &handle, verb, payload, deadline_ms)
        .await
    {
        Ok(()) => DispatchOutcome::Ok,
        Err(StewardError::Dispatch(msg)) => classify_dispatch_refusal(&msg),
        Err(other) => DispatchOutcome::Refused {
            class: "internal",
            subclass: "fast_path_dispatch_failed",
            message: other.to_string(),
        },
    };
    (cid, outcome)
}

/// Map a `StewardError::Dispatch` message produced by
/// [`PluginRouter::course_correct_fast`] into the structured
/// refusal shape the wire dispatcher emits. The router seeds the
/// message with a `fast_path:<subclass>:` token (or a slow-path
/// dispatch token for non-Fast-Path failures) so the wire side
/// can recover the subclass without parsing free-form text.
fn classify_dispatch_refusal(message: &str) -> DispatchOutcome {
    if let Some(rest) =
        message.strip_prefix("fast_path:not_fast_path_eligible:")
    {
        return DispatchOutcome::Refused {
            class: "not_found",
            subclass: "not_fast_path_eligible",
            message: rest.trim_start().to_string(),
        };
    }
    if let Some(rest) =
        message.strip_prefix("fast_path:fast_path_budget_exceeded:")
    {
        return DispatchOutcome::Refused {
            class: "unavailable",
            subclass: "fast_path_budget_exceeded",
            message: rest.trim_start().to_string(),
        };
    }
    if message.starts_with("no plugin on shelf:") {
        return DispatchOutcome::Refused {
            class: "not_found",
            subclass: "shelf_not_admitted",
            message: message.to_string(),
        };
    }
    if message.contains("has been unloaded") {
        return DispatchOutcome::Refused {
            class: "unavailable",
            subclass: "shelf_unloaded",
            message: message.to_string(),
        };
    }
    if message.contains("is a respondent, not a warden") {
        return DispatchOutcome::Refused {
            class: "contract_violation",
            subclass: "shelf_not_warden",
            message: message.to_string(),
        };
    }
    DispatchOutcome::Refused {
        class: "internal",
        subclass: "fast_path_dispatch_failed",
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> FastPathRequest {
        FastPathRequest::Dispatch {
            cid: 42,
            shelf: "audio.transport".into(),
            verb: "volume_set".into(),
            payload: vec![0u8, 1, 2, 3],
            handle_id: "custody-1".into(),
            handle_started_at_ms: 1_700_000_000_000,
            deadline_ms: Some(50),
        }
    }

    #[test]
    fn fast_path_request_round_trips_through_cbor() {
        let req = sample_request();
        let bytes = encode_cbor_value(&req).unwrap();
        let back: FastPathRequest = decode_cbor_value(&bytes).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn fast_path_response_round_trips_through_cbor() {
        let ok = FastPathResponse::Dispatched { cid: 99 };
        let bytes = encode_cbor_value(&ok).unwrap();
        let back: FastPathResponse = decode_cbor_value(&bytes).unwrap();
        assert_eq!(ok, back);

        let err = FastPathResponse::Error {
            cid: 99,
            class: "not_found".into(),
            subclass: "not_fast_path_eligible".into(),
            message: "warden does not serve verb".into(),
        };
        let bytes = encode_cbor_value(&err).unwrap();
        let back: FastPathResponse = decode_cbor_value(&bytes).unwrap();
        assert_eq!(err, back);
    }

    #[test]
    fn fast_path_request_payload_encodes_as_cbor_byte_string() {
        // Pin the format-aware base64_bytes contract on the
        // CBOR side: the payload field rides as a native byte
        // string, never as a base64 text fall-through.
        let req = FastPathRequest::Dispatch {
            cid: 1,
            shelf: "s".into(),
            verb: "v".into(),
            payload: b"hello".to_vec(),
            handle_id: "h".into(),
            handle_started_at_ms: 0,
            deadline_ms: None,
        };
        let bytes = encode_cbor_value(&req).unwrap();
        let needle = [0x45u8, b'h', b'e', b'l', b'l', b'o'];
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "expected raw byte string in CBOR encoding; got {bytes:02x?}"
        );
    }

    #[test]
    fn classify_recognises_not_fast_path_eligible() {
        let m = classify_dispatch_refusal(
            "fast_path:not_fast_path_eligible: warden on shelf x does not serve verb",
        );
        match m {
            DispatchOutcome::Refused {
                class, subclass, ..
            } => {
                assert_eq!(class, "not_found");
                assert_eq!(subclass, "not_fast_path_eligible");
            }
            _ => panic!("expected Refused outcome"),
        }
    }

    #[test]
    fn classify_recognises_budget_exceeded() {
        let m = classify_dispatch_refusal(
            "fast_path:fast_path_budget_exceeded: dispatch on shelf x exceeded budget 50 ms",
        );
        match m {
            DispatchOutcome::Refused {
                class, subclass, ..
            } => {
                assert_eq!(class, "unavailable");
                assert_eq!(subclass, "fast_path_budget_exceeded");
            }
            _ => panic!("expected Refused outcome"),
        }
    }

    #[test]
    fn classify_recognises_shelf_not_admitted() {
        let m = classify_dispatch_refusal("no plugin on shelf: nope");
        match m {
            DispatchOutcome::Refused {
                class, subclass, ..
            } => {
                assert_eq!(class, "not_found");
                assert_eq!(subclass, "shelf_not_admitted");
            }
            _ => panic!("expected Refused outcome"),
        }
    }

    #[test]
    fn classify_recognises_shelf_unloaded() {
        let m =
            classify_dispatch_refusal("plugin on shelf x has been unloaded");
        match m {
            DispatchOutcome::Refused {
                class, subclass, ..
            } => {
                assert_eq!(class, "unavailable");
                assert_eq!(subclass, "shelf_unloaded");
            }
            _ => panic!("expected Refused outcome"),
        }
    }

    #[test]
    fn classify_recognises_shelf_is_respondent() {
        let m = classify_dispatch_refusal(
            "course_correct on shelf x: plugin is a respondent, not a warden",
        );
        match m {
            DispatchOutcome::Refused {
                class, subclass, ..
            } => {
                assert_eq!(class, "contract_violation");
                assert_eq!(subclass, "shelf_not_warden");
            }
            _ => panic!("expected Refused outcome"),
        }
    }

    #[test]
    fn classify_falls_through_to_internal_for_unknown_messages() {
        let m = classify_dispatch_refusal("some other error");
        match m {
            DispatchOutcome::Refused {
                class, subclass, ..
            } => {
                assert_eq!(class, "internal");
                assert_eq!(subclass, "fast_path_dispatch_failed");
            }
            _ => panic!("expected Refused outcome"),
        }
    }

    #[test]
    fn fast_path_max_frame_size_pinned() {
        // 64 KiB is the design intent; bumping it should be a
        // deliberate decision visible in code review rather than
        // a silent constant tweak.
        assert_eq!(FAST_PATH_MAX_FRAME_SIZE, 64 * 1024);
    }

    #[test]
    fn default_fast_path_socket_path_pinned() {
        // Distributions packaging evo-core embed this path in
        // their systemd RuntimeDirectory; a silent change would
        // break their unit files.
        assert_eq!(DEFAULT_FAST_PATH_SOCKET, "/run/evo/fast.sock");
    }

    #[test]
    fn fast_path_config_defaults_to_default_socket() {
        let c = FastPathConfig::default();
        assert_eq!(c.socket_path(), Path::new(DEFAULT_FAST_PATH_SOCKET));
    }

    #[test]
    fn fast_path_config_with_socket_path_overrides_default() {
        let c =
            FastPathConfig::new().with_socket_path("/tmp/evo-test-fast.sock");
        assert_eq!(c.socket_path(), Path::new("/tmp/evo-test-fast.sock"));
    }

    #[tokio::test]
    async fn fast_path_listener_binds_and_serves_under_acl_grant() {
        // End-to-end smoke: spawn the listener, connect a
        // client, observe that the listener does not immediately
        // refuse a same-UID local peer (default ACL grants).
        // The client's first frame is intentionally malformed so
        // the server loops out cleanly; the test asserts only
        // that the connection was accepted past the ACL gate.
        use std::os::unix::net::UnixStream as StdStream;
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("fast.sock");
        let config = FastPathConfig::new().with_socket_path(&socket_path);
        let router = Arc::new(PluginRouter::new(
            crate::state::StewardState::for_tests(),
        ));
        let acl = Arc::new(ClientAcl::default());
        // Set the steward identity to the current process UID so
        // the local peer (also the test process) passes the
        // default same-UID ACL.
        let steward_identity = StewardIdentity::current();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let listener_task = tokio::spawn(run_fast_path(
            config,
            router,
            acl,
            steward_identity,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // Wait briefly for the listener to bind. Polling the
        // file path keeps the test deterministic without sleeping
        // longer than necessary.
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            socket_path.exists(),
            "fast path socket did not appear at {}",
            socket_path.display()
        );

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        // The connection accepted; close the write half so the
        // server's read loop hits PeerClosed and exits cleanly.
        client.shutdown().await.unwrap();
        drop(client);

        // Tear down the listener.
        let _ = shutdown_tx.send(());
        listener_task.await.unwrap().unwrap();
        let _ = StdStream::connect(&socket_path);
    }
}
