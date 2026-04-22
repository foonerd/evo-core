//! The client-facing Unix socket server.
//!
//! v0 protocol is deliberately minimal: length-prefixed JSON frames over
//! a Unix domain socket. This is enough to prove the fabric
//! admits-and-dispatches end-to-end and serves federated subject
//! projections. The production protocol (SDK pass 3) is richer and
//! richer-typed; this is the skeleton version.
//!
//! ## Frame format
//!
//! ```text
//! [4-byte big-endian length] [length bytes of UTF-8 JSON]
//! ```
//!
//! ## Request JSON
//!
//! Every request carries an `op` discriminator.
//!
//! ### `op = "request"`: dispatch a plugin request
//!
//! ```json
//! { "op": "request",
//!   "shelf": "example.echo",
//!   "request_type": "echo",
//!   "payload_b64": "aGVsbG8=" }
//! ```
//!
//! ### `op = "project_subject"`: compose a federated subject projection
//!
//! ```json
//! { "op": "project_subject",
//!   "canonical_id": "<uuid>",
//!   "scope": {
//!     "relation_predicates": ["album_of"],
//!     "direction": "forward"
//!   } }
//! ```
//!
//! The `scope` field is optional; omitting it or omitting its sub-fields
//! yields a scope with no relation traversal (empty predicates, forward
//! direction).
//!
//! ### `op = "list_active_custodies"`: snapshot the custody ledger
//!
//! ```json
//! { "op": "list_active_custodies" }
//! ```
//!
//! v0 returns every active custody; no filter or scope arguments. A
//! future pass can add filtering (by plugin, by shelf) without
//! breaking this minimal shape.
//!
//! ## Response JSON
//!
//! Plugin-request success:
//! ```json
//! { "payload_b64": "aGVsbG8=" }
//! ```
//!
//! Projection success: the full [`SubjectProjection`] serialised as
//! described in `PROJECTIONS.md` section 4.4.
//!
//! `list_active_custodies` success:
//! ```json
//! { "active_custodies": [
//!     { "plugin": "org.example.warden",
//!       "handle_id": "custody-42",
//!       "shelf": "example.custody",
//!       "custody_type": "playback",
//!       "last_state": {
//!         "payload_b64": "cGxheWluZw==",
//!         "health": "healthy",
//!         "reported_at_ms": 1700000000000
//!       },
//!       "started_at_ms": 1700000000000,
//!       "last_updated_ms": 1700000000050 }
//! ] }
//! ```
//!
//! Any failure:
//! ```json
//! { "error": "no plugin on shelf: foo.bar" }
//! ```
//!
//! [`SubjectProjection`]: crate::projections::SubjectProjection

use crate::admission::AdmissionEngine;
use crate::custody::{CustodyRecord, StateSnapshot};
use crate::error::StewardError;
use crate::projections::{
    DegradedReason, DegradedReasonKind, ProjectionEngine, ProjectionError,
    ProjectionScope, RelatedSubject, RelationDirection, SubjectProjection,
};
use crate::relations::WalkDirection;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use evo_plugin_sdk::contract::{HealthStatus, Request};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Maximum size of a single JSON frame. Prevents malicious or malformed
/// clients from forcing the steward to allocate unbounded memory.
const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Source of correlation IDs assigned to incoming requests that do not
/// already carry one.
static NEXT_CID: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------
// Wire types - requests
// ---------------------------------------------------------------------

/// A client request as it appears on the wire.
///
/// Internally tagged: the `op` field selects the variant.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientRequest {
    /// Dispatch a plugin request on a specific shelf.
    Request {
        /// Fully-qualified shelf name (`<rack>.<shelf>`).
        shelf: String,
        /// Request type the plugin must handle.
        request_type: String,
        /// Base64-encoded request payload.
        #[serde(default)]
        payload_b64: String,
    },
    /// Compose a federated projection for a canonical subject.
    ProjectSubject {
        /// Canonical subject ID to project.
        canonical_id: String,
        /// Optional scope declaration. Defaults to no relation
        /// traversal.
        #[serde(default)]
        scope: ProjectionScopeWire,
    },
    /// Snapshot the custody ledger - every currently-held custody the
    /// steward has recorded. No fields; v0 returns everything.
    ListActiveCustodies,
}

/// Wire form of [`ProjectionScope`]. See module-level docs for JSON
/// shape.
#[derive(Debug, Default, Deserialize)]
struct ProjectionScopeWire {
    /// Relation predicates to traverse. Empty means no relation
    /// traversal.
    #[serde(default)]
    relation_predicates: Vec<String>,
    /// Walk direction.
    #[serde(default)]
    direction: WalkDirectionWire,
    /// Maximum walk depth. Absent means the domain default
    /// ([`crate::projections::DEFAULT_MAX_DEPTH`]).
    #[serde(default)]
    max_depth: Option<usize>,
    /// Maximum visit count. Absent means the domain default
    /// ([`crate::projections::DEFAULT_MAX_VISITS`]).
    #[serde(default)]
    max_visits: Option<usize>,
}

/// Wire form of [`WalkDirection`]. Accepts `forward`, `inverse`, or
/// `both`. Defaults to `forward`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WalkDirectionWire {
    /// Follow edges where the subject is the source.
    #[default]
    Forward,
    /// Follow edges where the subject is the target.
    Inverse,
    /// Follow both directions.
    Both,
}

impl From<WalkDirectionWire> for WalkDirection {
    fn from(w: WalkDirectionWire) -> Self {
        match w {
            WalkDirectionWire::Forward => WalkDirection::Forward,
            WalkDirectionWire::Inverse => WalkDirection::Inverse,
            WalkDirectionWire::Both => WalkDirection::Both,
        }
    }
}

impl From<ProjectionScopeWire> for ProjectionScope {
    fn from(w: ProjectionScopeWire) -> Self {
        let mut scope = ProjectionScope {
            relation_predicates: w.relation_predicates,
            direction: w.direction.into(),
            max_depth: crate::projections::DEFAULT_MAX_DEPTH,
            max_visits: crate::projections::DEFAULT_MAX_VISITS,
        };
        if let Some(d) = w.max_depth {
            scope.max_depth = d;
        }
        if let Some(v) = w.max_visits {
            scope.max_visits = v;
        }
        scope
    }
}

// ---------------------------------------------------------------------
// Wire types - responses
// ---------------------------------------------------------------------

/// A response as it appears on the wire. Untagged: the variant is
/// disambiguated by the distinct top-level fields of each shape
/// (`payload_b64` for plugin success, `canonical_id`+`subject_type`
/// for projections, `active_custodies` for the ledger snapshot,
/// `error` for failures).
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ClientResponse {
    /// Plugin-request success.
    Success {
        /// Base64-encoded response payload.
        payload_b64: String,
    },
    /// Projection success.
    Projection(SubjectProjectionWire),
    /// Active custodies snapshot. Shape:
    /// `{ "active_custodies": [ <CustodyRecordWire>, ... ] }`.
    ActiveCustodies {
        /// Every currently-held custody in the steward's ledger.
        /// Order is unspecified (the ledger is a HashMap).
        active_custodies: Vec<CustodyRecordWire>,
    },
    /// Any failure.
    Error {
        /// Human-readable error message.
        error: String,
    },
}

/// Wire form of [`SubjectProjection`]. Mirrors the domain shape but
/// serialises `composed_at` as milliseconds since the UNIX epoch and
/// converts enumerations to snake_case strings.
#[derive(Debug, Serialize)]
struct SubjectProjectionWire {
    canonical_id: String,
    subject_type: String,
    addressings: Vec<AddressingEntryWire>,
    related: Vec<RelatedSubjectWire>,
    /// Composition timestamp, milliseconds since the UNIX epoch.
    composed_at_ms: u64,
    shape_version: u32,
    claimants: Vec<String>,
    degraded: bool,
    degraded_reasons: Vec<DegradedReasonWire>,
    /// True if the relation walk was truncated at or before the point
    /// this projection was built. See
    /// [`SubjectProjection::walk_truncated`].
    walk_truncated: bool,
}

#[derive(Debug, Serialize)]
struct AddressingEntryWire {
    scheme: String,
    value: String,
    claimant: String,
}

#[derive(Debug, Serialize)]
struct RelatedSubjectWire {
    predicate: String,
    /// `"forward"` or `"inverse"`.
    direction: &'static str,
    target_id: String,
    /// Subject type of the other end. `null` indicates a dangling
    /// relation (the edge target is not in the subject registry).
    target_type: Option<String>,
    relation_claimants: Vec<String>,
    /// Recursively composed projection for the other end of the edge,
    /// when the scope's `max_depth` permitted expansion. `null`
    /// otherwise. See [`RelatedSubject::nested`] for the non-expansion
    /// rules.
    nested: Option<Box<SubjectProjectionWire>>,
}

#[derive(Debug, Serialize)]
struct DegradedReasonWire {
    /// `"dangling_relation"`, etc.
    kind: &'static str,
    detail: Option<String>,
}

impl From<SubjectProjection> for SubjectProjectionWire {
    fn from(p: SubjectProjection) -> Self {
        Self {
            canonical_id: p.canonical_id,
            subject_type: p.subject_type,
            addressings: p
                .addressings
                .into_iter()
                .map(|a| AddressingEntryWire {
                    scheme: a.scheme,
                    value: a.value,
                    claimant: a.claimant,
                })
                .collect(),
            related: p.related.into_iter().map(Into::into).collect(),
            composed_at_ms: system_time_to_ms(p.composed_at),
            shape_version: p.shape_version,
            claimants: p.claimants,
            degraded: p.degraded,
            degraded_reasons: p
                .degraded_reasons
                .into_iter()
                .map(Into::into)
                .collect(),
            walk_truncated: p.walk_truncated,
        }
    }
}

impl From<RelatedSubject> for RelatedSubjectWire {
    fn from(r: RelatedSubject) -> Self {
        Self {
            predicate: r.predicate,
            direction: match r.direction {
                RelationDirection::Forward => "forward",
                RelationDirection::Inverse => "inverse",
            },
            target_id: r.target_id,
            target_type: r.target_type,
            relation_claimants: r.relation_claimants,
            nested: r
                .nested
                .map(|boxed| Box::new(SubjectProjectionWire::from(*boxed))),
        }
    }
}

impl From<DegradedReason> for DegradedReasonWire {
    fn from(d: DegradedReason) -> Self {
        Self {
            kind: match d.kind {
                DegradedReasonKind::DanglingRelation => "dangling_relation",
            },
            detail: d.detail,
        }
    }
}

// ---------------------------------------------------------------------
// Wire types - custody ledger
// ---------------------------------------------------------------------

/// Wire form of [`CustodyRecord`]. Mirrors the domain shape but
/// serialises timestamps as milliseconds since the UNIX epoch.
/// `HealthStatus` comes through verbatim because the SDK type
/// already serialises as a lowercase string.
#[derive(Debug, Serialize)]
struct CustodyRecordWire {
    /// Canonical name of the warden plugin holding this custody.
    plugin: String,
    /// Warden-chosen handle id. Opaque to the steward.
    handle_id: String,
    /// Fully-qualified shelf, once recorded.
    shelf: Option<String>,
    /// Custody type the Assignment was tagged with, once recorded.
    custody_type: Option<String>,
    /// Most recent state snapshot, if any reports have been seen.
    last_state: Option<StateSnapshotWire>,
    /// When the record was first created in the ledger, ms since
    /// UNIX epoch.
    started_at_ms: u64,
    /// When any field on the record was last changed, ms since
    /// UNIX epoch.
    last_updated_ms: u64,
}

/// Wire form of [`StateSnapshot`]. Payload is base64-encoded;
/// `health` uses the SDK's built-in serde form (lowercase string);
/// `reported_at` becomes `reported_at_ms`.
#[derive(Debug, Serialize)]
struct StateSnapshotWire {
    /// Base64-encoded opaque payload from the plugin's state report.
    payload_b64: String,
    /// Health declared by the plugin at report time.
    health: HealthStatus,
    /// When the steward recorded the report, ms since UNIX epoch.
    reported_at_ms: u64,
}

impl From<CustodyRecord> for CustodyRecordWire {
    fn from(r: CustodyRecord) -> Self {
        Self {
            plugin: r.plugin,
            handle_id: r.handle_id,
            shelf: r.shelf,
            custody_type: r.custody_type,
            last_state: r.last_state.map(Into::into),
            started_at_ms: system_time_to_ms(r.started_at),
            last_updated_ms: system_time_to_ms(r.last_updated),
        }
    }
}

impl From<StateSnapshot> for StateSnapshotWire {
    fn from(s: StateSnapshot) -> Self {
        Self {
            payload_b64: B64.encode(&s.payload),
            health: s.health,
            reported_at_ms: system_time_to_ms(s.reported_at),
        }
    }
}

/// Convert a `SystemTime` to milliseconds since the UNIX epoch.
/// Returns 0 for pre-epoch timestamps (should not occur in practice;
/// the steward records composition time via `SystemTime::now()`).
fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------

/// The Unix socket server.
pub struct Server {
    socket_path: PathBuf,
    engine: Arc<Mutex<AdmissionEngine>>,
    projections: Arc<ProjectionEngine>,
}

impl Server {
    /// Construct a server bound to a socket path and sharing an
    /// admission engine and a projection engine. The socket is not
    /// created until [`run`] is called.
    ///
    /// The [`ProjectionEngine`] must read from the same subject
    /// registry and relation graph as the admission engine; typically
    /// constructed as:
    ///
    /// ```ignore
    /// let projections = Arc::new(ProjectionEngine::new(
    ///     admission.registry(),
    ///     admission.relation_graph(),
    /// ));
    /// ```
    ///
    /// [`run`]: Self::run
    pub fn new(
        socket_path: PathBuf,
        engine: Arc<Mutex<AdmissionEngine>>,
        projections: Arc<ProjectionEngine>,
    ) -> Self {
        Self {
            socket_path,
            engine,
            projections,
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
                            let projections = Arc::clone(&self.projections);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(
                                    stream, engine, projections
                                ).await {
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
    projections: Arc<ProjectionEngine>,
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

        let response = process_frame(&body, &engine, &projections).await;

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

/// Parse, dispatch, and produce a [`ClientResponse`] for one frame.
///
/// Never panics; parse failures and dispatch failures both yield an
/// error response rather than bubbling up to close the connection.
async fn process_frame(
    body: &[u8],
    engine: &Arc<Mutex<AdmissionEngine>>,
    projections: &Arc<ProjectionEngine>,
) -> ClientResponse {
    let req: ClientRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return ClientResponse::Error {
                error: format!("invalid JSON: {e}"),
            };
        }
    };

    match req {
        ClientRequest::Request {
            shelf,
            request_type,
            payload_b64,
        } => handle_plugin_request(
            engine, shelf, request_type, payload_b64,
        )
        .await,
        ClientRequest::ProjectSubject {
            canonical_id,
            scope,
        } => handle_project_subject(projections, canonical_id, scope),
        ClientRequest::ListActiveCustodies => {
            handle_list_active_custodies(engine).await
        }
    }
}

/// Dispatch a plugin request (`op = "request"`).
async fn handle_plugin_request(
    engine: &Arc<Mutex<AdmissionEngine>>,
    shelf: String,
    request_type: String,
    payload_b64: String,
) -> ClientResponse {
    let payload = match B64.decode(&payload_b64) {
        Ok(p) => p,
        Err(e) => {
            return ClientResponse::Error {
                error: format!("invalid base64 payload: {e}"),
            };
        }
    };

    let cid = NEXT_CID.fetch_add(1, Ordering::Relaxed);
    let sdk_request = Request {
        request_type,
        payload,
        correlation_id: cid,
        deadline: None,
    };

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

/// Compose and emit a subject projection (`op = "project_subject"`).
fn handle_project_subject(
    projections: &Arc<ProjectionEngine>,
    canonical_id: String,
    scope: ProjectionScopeWire,
) -> ClientResponse {
    let scope: ProjectionScope = scope.into();
    match projections.project_subject(&canonical_id, &scope) {
        Ok(p) => ClientResponse::Projection(p.into()),
        Err(ProjectionError::UnknownSubject(id)) => ClientResponse::Error {
            error: format!("unknown subject: {id}"),
        },
    }
}

/// Snapshot the custody ledger (`op = "list_active_custodies"`).
///
/// Briefly locks the admission engine to clone the ledger Arc, then
/// queries the ledger without holding the engine lock. The ledger
/// has its own RwLock; contention with in-flight custody ops is
/// limited to that.
async fn handle_list_active_custodies(
    engine: &Arc<Mutex<AdmissionEngine>>,
) -> ClientResponse {
    let ledger = {
        let guard = engine.lock().await;
        guard.custody_ledger()
    };
    let active_custodies: Vec<CustodyRecordWire> = ledger
        .list_active()
        .into_iter()
        .map(Into::into)
        .collect();
    ClientResponse::ActiveCustodies { active_custodies }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;

    #[test]
    fn client_request_parses_request_op() {
        let json = r#"{"op":"request","shelf":"a.b","request_type":"t","payload_b64":"aGVsbG8="}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::Request {
                shelf,
                request_type,
                payload_b64,
            } => {
                assert_eq!(shelf, "a.b");
                assert_eq!(request_type, "t");
                assert_eq!(B64.decode(payload_b64).unwrap(), b"hello");
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn client_request_payload_defaults_empty() {
        let json = r#"{"op":"request","shelf":"a.b","request_type":"t"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::Request { payload_b64, .. } => {
                assert_eq!(payload_b64, "");
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_project_subject_minimal() {
        let json = r#"{"op":"project_subject","canonical_id":"abc-123"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectSubject {
                canonical_id,
                scope,
            } => {
                assert_eq!(canonical_id, "abc-123");
                assert!(scope.relation_predicates.is_empty());
                assert!(matches!(scope.direction, WalkDirectionWire::Forward));
            }
            other => panic!("expected ProjectSubject, got {other:?}"),
        }
    }

    #[test]
    fn client_request_parses_project_subject_with_scope() {
        let json = r#"{
            "op": "project_subject",
            "canonical_id": "abc-123",
            "scope": {
                "relation_predicates": ["album_of", "performed_by"],
                "direction": "both"
            }
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectSubject {
                canonical_id,
                scope,
            } => {
                assert_eq!(canonical_id, "abc-123");
                assert_eq!(scope.relation_predicates.len(), 2);
                assert!(scope.relation_predicates.contains(&"album_of".to_string()));
                assert!(matches!(scope.direction, WalkDirectionWire::Both));
            }
            other => panic!("expected ProjectSubject, got {other:?}"),
        }
    }

    #[test]
    fn client_request_rejects_unknown_op() {
        let json = r#"{"op":"who_knows","canonical_id":"abc"}"#;
        assert!(serde_json::from_str::<ClientRequest>(json).is_err());
    }

    #[test]
    fn client_request_rejects_missing_op() {
        let json = r#"{"shelf":"a.b","request_type":"t"}"#;
        assert!(serde_json::from_str::<ClientRequest>(json).is_err());
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

    #[test]
    fn client_response_projection_serialises() {
        let p = SubjectProjectionWire {
            canonical_id: "abc".into(),
            subject_type: "track".into(),
            addressings: vec![AddressingEntryWire {
                scheme: "s".into(),
                value: "v".into(),
                claimant: "p".into(),
            }],
            related: vec![],
            composed_at_ms: 1234567890,
            shape_version: 1,
            claimants: vec!["p".into()],
            degraded: false,
            degraded_reasons: vec![],
            walk_truncated: false,
        };
        let r = ClientResponse::Projection(p);
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("canonical_id"));
        assert!(s.contains("subject_type"));
        assert!(s.contains("composed_at_ms"));
        assert!(s.contains("walk_truncated"));
        assert!(!s.contains("payload_b64"));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn walk_direction_wire_maps_to_domain() {
        assert!(matches!(
            WalkDirection::from(WalkDirectionWire::Forward),
            WalkDirection::Forward
        ));
        assert!(matches!(
            WalkDirection::from(WalkDirectionWire::Inverse),
            WalkDirection::Inverse
        ));
        assert!(matches!(
            WalkDirection::from(WalkDirectionWire::Both),
            WalkDirection::Both
        ));
    }

    #[test]
    fn projection_scope_wire_maps_to_domain() {
        let w = ProjectionScopeWire {
            relation_predicates: vec!["a".into(), "b".into()],
            direction: WalkDirectionWire::Inverse,
            max_depth: None,
            max_visits: None,
        };
        let d: ProjectionScope = w.into();
        assert_eq!(d.relation_predicates, vec!["a", "b"]);
        assert!(matches!(d.direction, WalkDirection::Inverse));
        assert_eq!(d.max_depth, crate::projections::DEFAULT_MAX_DEPTH);
        assert_eq!(d.max_visits, crate::projections::DEFAULT_MAX_VISITS);
    }

    #[test]
    fn projection_scope_wire_overrides_depth_and_visits() {
        let w = ProjectionScopeWire {
            relation_predicates: vec!["a".into()],
            direction: WalkDirectionWire::Forward,
            max_depth: Some(7),
            max_visits: Some(42),
        };
        let d: ProjectionScope = w.into();
        assert_eq!(d.max_depth, 7);
        assert_eq!(d.max_visits, 42);
    }

    #[test]
    fn client_request_parses_project_subject_with_depth() {
        let json = r#"{
            "op": "project_subject",
            "canonical_id": "abc",
            "scope": {
                "relation_predicates": ["album_of"],
                "direction": "forward",
                "max_depth": 3,
                "max_visits": 100
            }
        }"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        match r {
            ClientRequest::ProjectSubject { scope, .. } => {
                assert_eq!(scope.max_depth, Some(3));
                assert_eq!(scope.max_visits, Some(100));
            }
            other => panic!("expected ProjectSubject, got {other:?}"),
        }
    }

    #[test]
    fn system_time_to_ms_handles_epoch() {
        assert_eq!(system_time_to_ms(UNIX_EPOCH), 0);
    }

    #[test]
    fn system_time_to_ms_handles_future() {
        let one_second_past_epoch =
            UNIX_EPOCH + std::time::Duration::from_secs(1);
        assert_eq!(system_time_to_ms(one_second_past_epoch), 1000);
    }

    // -----------------------------------------------------------------
    // list_active_custodies tests (pass 4h).
    // -----------------------------------------------------------------

    #[test]
    fn client_request_parses_list_active_custodies() {
        let json = r#"{"op":"list_active_custodies"}"#;
        let r: ClientRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(r, ClientRequest::ListActiveCustodies));
    }

    #[test]
    fn client_response_active_custodies_empty_serialises() {
        let r = ClientResponse::ActiveCustodies {
            active_custodies: vec![],
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["active_custodies"].is_array());
        assert_eq!(v["active_custodies"].as_array().unwrap().len(), 0);
        // Distinctive key for the untagged enum variant.
        assert!(!s.contains("payload_b64"));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn client_response_active_custodies_populated_serialises() {
        let rec = CustodyRecordWire {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: Some("example.custody".into()),
            custody_type: Some("playback".into()),
            last_state: Some(StateSnapshotWire {
                payload_b64: B64.encode(b"state=playing"),
                health: HealthStatus::Healthy,
                reported_at_ms: 1_700_000_000_050,
            }),
            started_at_ms: 1_700_000_000_000,
            last_updated_ms: 1_700_000_000_050,
        };
        let r = ClientResponse::ActiveCustodies {
            active_custodies: vec![rec],
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let arr = v["active_custodies"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let first = &arr[0];
        assert_eq!(first["plugin"].as_str(), Some("org.test.warden"));
        assert_eq!(first["handle_id"].as_str(), Some("c-1"));
        assert_eq!(first["shelf"].as_str(), Some("example.custody"));
        assert_eq!(first["custody_type"].as_str(), Some("playback"));
        assert_eq!(
            first["last_state"]["health"].as_str(),
            Some("healthy"),
            "HealthStatus should serialise lowercase via the SDK derive"
        );
        assert_eq!(
            first["last_state"]["reported_at_ms"].as_u64(),
            Some(1_700_000_000_050)
        );
        assert_eq!(
            first["started_at_ms"].as_u64(),
            Some(1_700_000_000_000)
        );
        let decoded = B64
            .decode(first["last_state"]["payload_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"state=playing");
    }

    #[test]
    fn custody_record_wire_from_full_record() {
        let snap = StateSnapshot {
            payload: b"state=x".to_vec(),
            health: HealthStatus::Degraded,
            reported_at: UNIX_EPOCH
                + std::time::Duration::from_millis(500),
        };
        let rec = CustodyRecord {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: Some("example.custody".into()),
            custody_type: Some("playback".into()),
            last_state: Some(snap),
            started_at: UNIX_EPOCH
                + std::time::Duration::from_millis(100),
            last_updated: UNIX_EPOCH
                + std::time::Duration::from_millis(500),
        };
        let wire: CustodyRecordWire = rec.into();
        assert_eq!(wire.plugin, "org.test.warden");
        assert_eq!(wire.handle_id, "c-1");
        assert_eq!(wire.shelf.as_deref(), Some("example.custody"));
        assert_eq!(wire.custody_type.as_deref(), Some("playback"));
        assert_eq!(wire.started_at_ms, 100);
        assert_eq!(wire.last_updated_ms, 500);
        let state = wire.last_state.expect("state");
        assert_eq!(
            B64.decode(&state.payload_b64).unwrap(),
            b"state=x"
        );
        assert_eq!(state.health, HealthStatus::Degraded);
        assert_eq!(state.reported_at_ms, 500);
    }

    #[test]
    fn custody_record_wire_from_partial_record() {
        // Simulates the state-report-first branch where record_custody
        // has not yet filled in shelf/custody_type.
        let rec = CustodyRecord {
            plugin: "org.test.warden".into(),
            handle_id: "c-1".into(),
            shelf: None,
            custody_type: None,
            last_state: None,
            started_at: UNIX_EPOCH
                + std::time::Duration::from_millis(100),
            last_updated: UNIX_EPOCH
                + std::time::Duration::from_millis(100),
        };
        let wire: CustodyRecordWire = rec.into();
        assert!(wire.shelf.is_none());
        assert!(wire.custody_type.is_none());
        assert!(wire.last_state.is_none());

        // Optionals serialise as null, not as missing keys, which is
        // more predictable for consumers.
        let s = serde_json::to_string(&wire).unwrap();
        assert!(s.contains("\"shelf\":null"));
        assert!(s.contains("\"custody_type\":null"));
        assert!(s.contains("\"last_state\":null"));
    }

    #[test]
    fn state_snapshot_wire_from_conversion() {
        let snap = StateSnapshot {
            payload: b"xyz".to_vec(),
            health: HealthStatus::Unhealthy,
            reported_at: UNIX_EPOCH
                + std::time::Duration::from_millis(2_500),
        };
        let wire: StateSnapshotWire = snap.into();
        assert_eq!(B64.decode(&wire.payload_b64).unwrap(), b"xyz");
        assert_eq!(wire.health, HealthStatus::Unhealthy);
        assert_eq!(wire.reported_at_ms, 2_500);
    }
}
