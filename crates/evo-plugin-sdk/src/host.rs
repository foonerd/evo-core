//! Plugin-side wire server ("host").
//!
//! Drives a `Plugin + Respondent` over a single `AsyncRead + AsyncWrite`
//! connection (Unix socket, tokio::io::duplex, TCP - anything that
//! implements the async I/O traits). Handles the full protocol lifecycle
//! per `docs/engineering/PLUGIN_CONTRACT.md` sections 6 through 11:
//!
//! - Reads framed JSON wire messages from the reader.
//! - Validates envelope fields: protocol version and plugin name on
//!   every frame.
//! - Dispatches requests (`describe`, `load`, `unload`, `health_check`,
//!   `handle_request`) to the plugin's trait methods.
//! - Sends responses (or structured `Error` frames on failure) back to
//!   the steward via a writer task.
//! - Builds a `LoadContext` with wire-backed callback implementations so
//!   the plugin's async events (`report_state`, subject/relation
//!   announcements and retractions) reach the steward as events on the
//!   same stream.
//!
//! ## Architecture
//!
//! A writer task owns the writer half of the connection. It consumes
//! a `tokio::sync::mpsc::Receiver<WireFrame>`; every frame emitted by
//! either the main dispatch loop or one of the callback implementations
//! is sent through this channel, and the writer task drains it in order.
//!
//! The main task owns the plugin and the reader half. It reads frames,
//! validates, dispatches to plugin trait methods, formats responses, and
//! sends them to the channel.
//!
//! Callback implementations ([`WireStateReporter`],
//! [`WireSubjectAnnouncer`], [`WireRelationAnnouncer`]) hold cloned
//! `Sender` handles. When the plugin calls a callback, the implementation
//! pushes a frame to the channel and the writer task forwards it.
//!
//! ## Deferred
//!
//! Factory verbs (`announce_instance`, `retract_instance`), warden verbs
//! (`take_custody` etc), and user-interaction requests have no wire
//! representation in this SDK version. Their callback implementations
//! ([`WireInstanceAnnouncer`], [`WireUserInteractionRequester`]) return
//! `ReportError::Invalid` so plugins that try to use them on a wire
//! transport get a clear error.
//!
//! ## Concurrency
//!
//! The main dispatch loop is sequential: one request in flight at a
//! time. Wardens would need to change this; respondents do not. Events
//! from callbacks race with request handling; the mpsc channel
//! serialises them into a single totally-ordered write stream.

use crate::codec::{read_frame_json, write_frame_json, WireError};
use crate::contract::{
    CallDeadline, ExternalAddressing, InstanceAnnouncement, InstanceAnnouncer,
    InstanceId, LoadContext, Plugin, PluginError, RelationAnnouncer,
    RelationAssertion, RelationRetraction, ReportError, ReportPriority,
    Request, Respondent, StateReporter, SubjectAnnouncement, SubjectAnnouncer,
    UserInteraction, UserInteractionRequester,
};
use crate::wire::{WireFrame, PROTOCOL_VERSION};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// Default event channel capacity. Events beyond this are backpressured;
/// a plugin that floods the channel will see its callback futures
/// pend until the writer task drains.
pub const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Errors raised by the host.
///
/// These are errors in the host machinery itself - the plugin's own
/// errors are mapped to `Error` wire frames and surfaced to the
/// steward, not reported here.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HostError {
    /// Wire codec or framing error.
    #[error("wire: {0}")]
    Wire(#[from] WireError),

    /// Protocol violation from the peer (steward): unexpected frame
    /// direction, frame of wrong variant for current state, etc.
    #[error("protocol violation: {0}")]
    Protocol(String),

    /// The peer sent a frame with a plugin name not matching the one
    /// the host was configured with.
    #[error("plugin name mismatch: expected '{expected}', got '{actual}'")]
    PluginMismatch {
        /// The plugin name the host was configured with.
        expected: String,
        /// The plugin name carried in the peer's frame.
        actual: String,
    },

    /// The peer sent a frame with a protocol version the host does not
    /// speak.
    #[error("protocol version mismatch: expected {expected}, got {actual}")]
    VersionMismatch {
        /// The protocol version the host speaks.
        expected: u16,
        /// The protocol version the peer announced.
        actual: u16,
    },

    /// The writer task failed. This is typically due to the transport
    /// closing unexpectedly; the string carries the underlying cause.
    #[error("writer task failed: {0}")]
    WriterTask(String),
}

/// Configuration for a host connection.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Canonical plugin name. Every wire frame's `plugin` field is
    /// validated against this.
    pub plugin_name: String,
    /// Capacity of the event channel. Smaller values apply more
    /// backpressure; larger values tolerate bursts of events at the
    /// cost of memory.
    pub event_channel_capacity: usize,
}

impl HostConfig {
    /// Construct a config with the default event channel capacity.
    pub fn new(plugin_name: impl Into<String>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
            event_channel_capacity: DEFAULT_EVENT_CHANNEL_CAPACITY,
        }
    }

    /// Override the event channel capacity.
    pub fn with_event_channel_capacity(mut self, capacity: usize) -> Self {
        self.event_channel_capacity = capacity;
        self
    }
}

/// Serve one plugin connection end-to-end.
///
/// Consumes the plugin, runs the protocol loop until the peer closes
/// the connection (cleanly, via `unload` then disconnect, or abruptly),
/// and returns. A successful return means the protocol completed without
/// a host-level error; the plugin may still have returned errors for
/// individual verbs, which were sent to the steward as `Error` frames.
///
/// The `reader` and `writer` are typically the halves of a split stream:
/// `let (r, w) = tokio::io::split(stream);`
pub async fn serve<P, R, W>(
    plugin: P,
    config: HostConfig,
    mut reader: R,
    writer: W,
) -> Result<(), HostError>
where
    P: Plugin + Respondent + 'static,
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let (tx, rx) = mpsc::channel::<WireFrame>(config.event_channel_capacity);
    let writer_task = tokio::spawn(writer_loop(writer, rx));

    let result = dispatch_loop(plugin, &config, &mut reader, tx).await;

    // Whatever happened, wait for the writer task so we don't leak it.
    // If our dispatch errored, the tx sender is already dropped (the
    // closure owns it). If we exited cleanly by the peer closing, the
    // sender is dropped at the end of dispatch_loop and the writer
    // drains naturally.
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if result.is_ok() {
                return Err(HostError::WriterTask(format!("{e}")));
            }
            // Preserve the earlier error; log the writer failure.
            tracing::warn!(error = %e, "writer task failed after dispatch error");
        }
        Err(join_err) => {
            if result.is_ok() {
                return Err(HostError::WriterTask(format!(
                    "writer task panicked: {join_err}"
                )));
            }
            tracing::warn!(
                error = %join_err,
                "writer task panicked after dispatch error"
            );
        }
    }

    result
}

async fn writer_loop<W>(
    mut writer: W,
    mut rx: mpsc::Receiver<WireFrame>,
) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        write_frame_json(&mut writer, &frame).await?;
    }
    Ok(())
}

async fn dispatch_loop<P, R>(
    mut plugin: P,
    config: &HostConfig,
    reader: &mut R,
    tx: mpsc::Sender<WireFrame>,
) -> Result<(), HostError>
where
    P: Plugin + Respondent + 'static,
    R: AsyncRead + Unpin,
{
    let event_cid = Arc::new(AtomicU64::new(1));

    loop {
        let frame = match read_frame_json(reader).await {
            Ok(f) => f,
            Err(WireError::PeerClosed) => {
                // Clean EOF from the peer. Drop our sender so the
                // writer task can drain any pending events and exit.
                drop(tx);
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        let (v, _cid, peer_plugin) = frame.envelope();
        if v != PROTOCOL_VERSION {
            return Err(HostError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                actual: v,
            });
        }
        if peer_plugin != config.plugin_name {
            return Err(HostError::PluginMismatch {
                expected: config.plugin_name.clone(),
                actual: peer_plugin.to_string(),
            });
        }

        if !frame.is_request() {
            return Err(HostError::Protocol(format!(
                "expected request frame, got {}",
                variant_name(&frame)
            )));
        }

        let response =
            handle_frame(&mut plugin, frame, config, &tx, &event_cid).await;

        if tx.send(response).await.is_err() {
            // Writer task closed - nothing we can do but return.
            return Err(HostError::Protocol(
                "writer task closed before response could be sent".into(),
            ));
        }
    }
}

async fn handle_frame<P>(
    plugin: &mut P,
    frame: WireFrame,
    config: &HostConfig,
    tx: &mpsc::Sender<WireFrame>,
    event_cid: &Arc<AtomicU64>,
) -> WireFrame
where
    P: Plugin + Respondent + 'static,
{
    match frame {
        WireFrame::Describe { v, cid, plugin: p } => {
            let description = plugin.describe().await;
            WireFrame::DescribeResponse {
                v,
                cid,
                plugin: p,
                description,
            }
        }

        WireFrame::Load {
            v,
            cid,
            plugin: p,
            config: cfg,
            state_dir,
            credentials_dir,
            deadline_ms,
        } => {
            let ctx = match build_load_context(
                cfg,
                state_dir,
                credentials_dir,
                deadline_ms,
                tx.clone(),
                event_cid.clone(),
                &p,
            ) {
                Ok(ctx) => ctx,
                Err(e) => {
                    return error_frame(v, cid, &p, e, true);
                }
            };

            match plugin.load(&ctx).await {
                Ok(()) => WireFrame::LoadResponse {
                    v,
                    cid,
                    plugin: p,
                },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::Unload { v, cid, plugin: p } => {
            match plugin.unload().await {
                Ok(()) => WireFrame::UnloadResponse {
                    v,
                    cid,
                    plugin: p,
                },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        WireFrame::HealthCheck { v, cid, plugin: p } => {
            let report = plugin.health_check().await;
            WireFrame::HealthCheckResponse {
                v,
                cid,
                plugin: p,
                report,
            }
        }

        WireFrame::HandleRequest {
            v,
            cid,
            plugin: p,
            request_type,
            payload,
            deadline_ms,
        } => {
            let deadline =
                deadline_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
            let req = Request {
                request_type,
                payload,
                correlation_id: cid,
                deadline,
            };
            match plugin.handle_request(&req).await {
                Ok(resp) => WireFrame::HandleRequestResponse {
                    v,
                    cid,
                    plugin: p,
                    payload: resp.payload,
                },
                Err(e) => plugin_error_to_frame(v, cid, &p, e),
            }
        }

        // These are caught by the is_request() filter above; unreachable.
        other => error_frame(
            other.envelope().0,
            other.envelope().1,
            &config.plugin_name,
            format!("unexpected frame: {}", variant_name(&other)),
            true,
        ),
    }
}

fn plugin_error_to_frame(
    v: u16,
    cid: u64,
    plugin: &str,
    err: PluginError,
) -> WireFrame {
    let fatal = err.is_fatal();
    error_frame(v, cid, plugin, format!("{err}"), fatal)
}

fn error_frame(
    v: u16,
    cid: u64,
    plugin: &str,
    message: impl Into<String>,
    fatal: bool,
) -> WireFrame {
    WireFrame::Error {
        v,
        cid,
        plugin: plugin.to_string(),
        message: message.into(),
        fatal,
    }
}

fn variant_name(frame: &WireFrame) -> &'static str {
    match frame {
        WireFrame::Describe { .. } => "describe",
        WireFrame::Load { .. } => "load",
        WireFrame::Unload { .. } => "unload",
        WireFrame::HealthCheck { .. } => "health_check",
        WireFrame::HandleRequest { .. } => "handle_request",
        WireFrame::DescribeResponse { .. } => "describe_response",
        WireFrame::LoadResponse { .. } => "load_response",
        WireFrame::UnloadResponse { .. } => "unload_response",
        WireFrame::HealthCheckResponse { .. } => "health_check_response",
        WireFrame::HandleRequestResponse { .. } => "handle_request_response",
        WireFrame::ReportState { .. } => "report_state",
        WireFrame::AnnounceSubject { .. } => "announce_subject",
        WireFrame::RetractSubject { .. } => "retract_subject",
        WireFrame::AssertRelation { .. } => "assert_relation",
        WireFrame::RetractRelation { .. } => "retract_relation",
        WireFrame::Error { .. } => "error",
    }
}

// ---------------------------------------------------------------------
// Config conversion
// ---------------------------------------------------------------------

fn build_load_context(
    config: serde_json::Value,
    state_dir: String,
    credentials_dir: String,
    deadline_ms: Option<u64>,
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    plugin_name: &str,
) -> Result<LoadContext, String> {
    let config = json_value_to_toml_table(config)
        .map_err(|e| format!("invalid config: {e}"))?;

    let state_reporter: Arc<dyn StateReporter> = Arc::new(WireStateReporter {
        tx: tx.clone(),
        event_cid: event_cid.clone(),
        plugin_name: plugin_name.to_string(),
    });
    let instance_announcer: Arc<dyn InstanceAnnouncer> =
        Arc::new(WireInstanceAnnouncer);
    let user_interaction_requester: Arc<dyn UserInteractionRequester> =
        Arc::new(WireUserInteractionRequester);
    let subject_announcer: Arc<dyn SubjectAnnouncer> =
        Arc::new(WireSubjectAnnouncer {
            tx: tx.clone(),
            event_cid: event_cid.clone(),
            plugin_name: plugin_name.to_string(),
        });
    let relation_announcer: Arc<dyn RelationAnnouncer> =
        Arc::new(WireRelationAnnouncer {
            tx,
            event_cid,
            plugin_name: plugin_name.to_string(),
        });

    let deadline =
        deadline_ms.map(|ms| CallDeadline(Instant::now() + Duration::from_millis(ms)));

    Ok(LoadContext {
        config,
        state_dir: PathBuf::from(state_dir),
        credentials_dir: PathBuf::from(credentials_dir),
        deadline,
        state_reporter,
        instance_announcer,
        user_interaction_requester,
        subject_announcer,
        relation_announcer,
    })
}

/// Convert a `serde_json::Value` to a `toml::Table`.
///
/// Explicit conversion to avoid nasty surprises: JSON nulls fail
/// loudly (TOML has no null). Numbers are mapped to TOML's `Integer`
/// when representable as `i64`, otherwise `Float`.
fn json_value_to_toml_table(
    v: serde_json::Value,
) -> Result<toml::Table, String> {
    match v {
        serde_json::Value::Object(map) => {
            let mut table = toml::Table::new();
            for (k, v) in map {
                table.insert(k, json_value_to_toml_value(v)?);
            }
            Ok(table)
        }
        other => Err(format!(
            "expected config object at top level, got {}",
            json_kind(&other)
        )),
    }
}

fn json_value_to_toml_value(
    v: serde_json::Value,
) -> Result<toml::Value, String> {
    Ok(match v {
        serde_json::Value::Null => {
            return Err(
                "JSON null is not representable in TOML config".to_string()
            );
        }
        serde_json::Value::Bool(b) => toml::Value::Boolean(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                return Err(format!("number out of range: {n}"));
            }
        }
        serde_json::Value::String(s) => toml::Value::String(s),
        serde_json::Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                out.push(json_value_to_toml_value(v)?);
            }
            toml::Value::Array(out)
        }
        serde_json::Value::Object(o) => {
            let mut t = toml::Table::new();
            for (k, v) in o {
                t.insert(k, json_value_to_toml_value(v)?);
            }
            toml::Value::Table(t)
        }
    })
}

fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------
// Wire-backed callbacks
// ---------------------------------------------------------------------

/// State reporter that pushes frames into the wire event channel.
#[derive(Debug)]
struct WireStateReporter {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    plugin_name: String,
}

impl StateReporter for WireStateReporter {
    fn report<'a>(
        &'a self,
        payload: Vec<u8>,
        priority: ReportPriority,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            tx.send(WireFrame::ReportState {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                payload,
                priority,
            })
            .await
            .map_err(|_| ReportError::ShuttingDown)
        })
    }
}

/// Subject announcer that pushes frames into the wire event channel.
#[derive(Debug)]
struct WireSubjectAnnouncer {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    plugin_name: String,
}

impl SubjectAnnouncer for WireSubjectAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: SubjectAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            tx.send(WireFrame::AnnounceSubject {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                announcement,
            })
            .await
            .map_err(|_| ReportError::ShuttingDown)
        })
    }

    fn retract<'a>(
        &'a self,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            tx.send(WireFrame::RetractSubject {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                addressing,
                reason,
            })
            .await
            .map_err(|_| ReportError::ShuttingDown)
        })
    }
}

/// Relation announcer that pushes frames into the wire event channel.
#[derive(Debug)]
struct WireRelationAnnouncer {
    tx: mpsc::Sender<WireFrame>,
    event_cid: Arc<AtomicU64>,
    plugin_name: String,
}

impl RelationAnnouncer for WireRelationAnnouncer {
    fn assert<'a>(
        &'a self,
        assertion: RelationAssertion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            tx.send(WireFrame::AssertRelation {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                assertion,
            })
            .await
            .map_err(|_| ReportError::ShuttingDown)
        })
    }

    fn retract<'a>(
        &'a self,
        retraction: RelationRetraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let tx = self.tx.clone();
        let plugin = self.plugin_name.clone();
        let cid = self.event_cid.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            tx.send(WireFrame::RetractRelation {
                v: PROTOCOL_VERSION,
                cid,
                plugin,
                retraction,
            })
            .await
            .map_err(|_| ReportError::ShuttingDown)
        })
    }
}

/// Placeholder instance announcer. Factory-on-wire is not yet
/// implemented; this stub returns `ReportError::Invalid` so plugins
/// that try to use factory semantics on a wire transport get a clear
/// error.
#[derive(Debug)]
struct WireInstanceAnnouncer;

const FACTORY_NOT_SUPPORTED: &str =
    "factory instance announcements are not yet supported on the wire transport";

impl InstanceAnnouncer for WireInstanceAnnouncer {
    fn announce<'a>(
        &'a self,
        _announcement: InstanceAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async {
            Err(ReportError::Invalid(FACTORY_NOT_SUPPORTED.into()))
        })
    }

    fn retract<'a>(
        &'a self,
        _instance_id: InstanceId,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async {
            Err(ReportError::Invalid(FACTORY_NOT_SUPPORTED.into()))
        })
    }
}

/// Placeholder user-interaction requester. Not yet implemented on the
/// wire transport; this stub returns `ReportError::Invalid` so plugins
/// that try to use it get a clear error.
#[derive(Debug)]
struct WireUserInteractionRequester;

const USER_INTERACTION_NOT_SUPPORTED: &str =
    "user-interaction requests are not yet supported on the wire transport";

impl UserInteractionRequester for WireUserInteractionRequester {
    fn request<'a>(
        &'a self,
        _interaction: UserInteraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async {
            Err(ReportError::Invalid(USER_INTERACTION_NOT_SUPPORTED.into()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        BuildInfo, HealthReport, HealthStatus, PluginDescription,
        PluginIdentity, Response, RuntimeCapabilities,
    };
    use crate::wire::PROTOCOL_VERSION;
    use std::sync::atomic::AtomicBool;
    use tokio::io::DuplexStream;

    // -----------------------------------------------------------------
    // Test plugin: records lifecycle calls, optionally emits events
    // during load.
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct TestPlugin {
        name: String,
        loaded: Arc<AtomicBool>,
        unloaded: Arc<AtomicBool>,
        // Callbacks the plugin should invoke during load. Borrowed
        // from the context at load time.
        announce_subject_on_load: Option<SubjectAnnouncement>,
        fail_load: bool,
        fail_handle_request: bool,
    }

    impl Plugin for TestPlugin {
        fn describe(
            &self,
        ) -> impl Future<Output = PluginDescription> + Send + '_
        {
            let name = self.name.clone();
            async move {
                PluginDescription {
                    identity: PluginIdentity {
                        name,
                        version: semver::Version::new(0, 1, 1),
                        contract: 1,
                    },
                    runtime_capabilities: RuntimeCapabilities {
                        request_types: vec!["echo".into()],
                        accepts_custody: false,
                        flags: Default::default(),
                    },
                    build_info: BuildInfo {
                        plugin_build: "test".into(),
                        sdk_version: crate::VERSION.into(),
                        rustc_version: None,
                        built_at: None,
                    },
                }
            }
        }

        fn load<'a>(
            &'a mut self,
            ctx: &'a LoadContext,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a
        {
            async move {
                if self.fail_load {
                    return Err(PluginError::Permanent("refused to load".into()));
                }
                if let Some(a) = &self.announce_subject_on_load {
                    ctx.subject_announcer
                        .announce(a.clone())
                        .await
                        .map_err(|e| {
                            PluginError::Permanent(format!("announce: {e}"))
                        })?;
                }
                self.loaded.store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        fn unload(
            &mut self,
        ) -> impl Future<Output = Result<(), PluginError>> + Send + '_
        {
            async move {
                self.unloaded.store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        fn health_check(
            &self,
        ) -> impl Future<Output = HealthReport> + Send + '_
        {
            async move {
                if self.loaded.load(Ordering::Relaxed) {
                    HealthReport::healthy()
                } else {
                    HealthReport::unhealthy("not loaded")
                }
            }
        }
    }

    impl Respondent for TestPlugin {
        fn handle_request<'a>(
            &'a mut self,
            req: &'a Request,
        ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a
        {
            async move {
                if self.fail_handle_request {
                    return Err(PluginError::Permanent("nope".into()));
                }
                Ok(Response::for_request(req, req.payload.clone()))
            }
        }
    }

    // Helper: create a duplex stream pair sized for test frames.
    fn duplex_pair() -> (DuplexStream, DuplexStream) {
        tokio::io::duplex(65536)
    }

    // -----------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn describe_roundtrip() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        // Send describe request.
        write_frame_json(
            &mut client_w,
            &WireFrame::Describe {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        // Read describe response.
        let resp = read_frame_json(&mut client_r).await.unwrap();
        match resp {
            WireFrame::DescribeResponse {
                v,
                cid,
                plugin,
                description,
            } => {
                assert_eq!(v, PROTOCOL_VERSION);
                assert_eq!(cid, 1);
                assert_eq!(plugin, "org.test.x");
                assert_eq!(description.identity.name, "org.test.x");
            }
            other => panic!("expected DescribeResponse, got {other:?}"),
        }

        // Cleanly close by dropping client.
        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn load_unload_lifecycle() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let loaded = Arc::new(AtomicBool::new(false));
        let unloaded = Arc::new(AtomicBool::new(false));

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            loaded: loaded.clone(),
            unloaded: unloaded.clone(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        // Load.
        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/state".into(),
                credentials_dir: "/tmp/creds".into(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::LoadResponse { cid, .. } => assert_eq!(cid, 1),
            other => panic!("expected LoadResponse, got {other:?}"),
        }
        assert!(loaded.load(Ordering::Relaxed));

        // Unload.
        write_frame_json(
            &mut client_w,
            &WireFrame::Unload {
                v: PROTOCOL_VERSION,
                cid: 2,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();
        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::UnloadResponse { cid, .. } => assert_eq!(cid, 2),
            other => panic!("expected UnloadResponse, got {other:?}"),
        }
        assert!(unloaded.load(Ordering::Relaxed));

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn handle_request_echoes_payload() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        write_frame_json(
            &mut client_w,
            &WireFrame::HandleRequest {
                v: PROTOCOL_VERSION,
                cid: 7,
                plugin: "org.test.x".into(),
                request_type: "echo".into(),
                payload: b"hello".to_vec(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::HandleRequestResponse { cid, payload, .. } => {
                assert_eq!(cid, 7);
                assert_eq!(payload, b"hello");
            }
            other => panic!("expected HandleRequestResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn health_check_when_unloaded_is_unhealthy() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        write_frame_json(
            &mut client_w,
            &WireFrame::HealthCheck {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::HealthCheckResponse { report, .. } => {
                assert_eq!(report.status, HealthStatus::Unhealthy);
            }
            other => panic!("expected HealthCheckResponse, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn load_failure_returns_error_frame() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            fail_load: true,
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/s".into(),
                credentials_dir: "/tmp/c".into(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        match read_frame_json(&mut client_r).await.unwrap() {
            WireFrame::Error { cid, fatal, message, .. } => {
                assert_eq!(cid, 1);
                assert!(!fatal);
                assert!(message.contains("refused to load"));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn plugin_name_mismatch_closes_connection() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        // Send a frame with the wrong plugin name.
        write_frame_json(
            &mut client_w,
            &WireFrame::Describe {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.attacker.y".into(),
            },
        )
        .await
        .unwrap();

        let _ = read_frame_json(&mut client_r).await;
        drop(client_w);

        let err = host.await.unwrap().unwrap_err();
        assert!(matches!(err, HostError::PluginMismatch { .. }));
    }

    #[tokio::test]
    async fn wrong_protocol_version_closes_connection() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        write_frame_json(
            &mut client_w,
            &WireFrame::Describe {
                v: 99,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        let _ = read_frame_json(&mut client_r).await;
        drop(client_w);

        let err = host.await.unwrap().unwrap_err();
        assert!(matches!(err, HostError::VersionMismatch { .. }));
    }

    #[tokio::test]
    async fn response_frame_from_peer_is_protocol_violation() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        // Send a LoadResponse (direction error: plugin emits, not
        // receives).
        write_frame_json(
            &mut client_w,
            &WireFrame::LoadResponse {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
            },
        )
        .await
        .unwrap();

        let _ = read_frame_json(&mut client_r).await;
        drop(client_w);

        let err = host.await.unwrap().unwrap_err();
        assert!(matches!(err, HostError::Protocol(_)));
    }

    #[tokio::test]
    async fn subject_announcement_during_load_reaches_wire() {
        let (client, server) = duplex_pair();
        let (server_r, server_w) = tokio::io::split(server);
        let (mut client_r, mut client_w) = tokio::io::split(client);

        let announcement = SubjectAnnouncement::new(
            "track",
            vec![ExternalAddressing::new("mpd-path", "/music/a.flac")],
        );

        let plugin = TestPlugin {
            name: "org.test.x".into(),
            announce_subject_on_load: Some(announcement.clone()),
            ..Default::default()
        };
        let host = tokio::spawn(serve(
            plugin,
            HostConfig::new("org.test.x"),
            server_r,
            server_w,
        ));

        write_frame_json(
            &mut client_w,
            &WireFrame::Load {
                v: PROTOCOL_VERSION,
                cid: 1,
                plugin: "org.test.x".into(),
                config: serde_json::json!({}),
                state_dir: "/tmp/s".into(),
                credentials_dir: "/tmp/c".into(),
                deadline_ms: None,
            },
        )
        .await
        .unwrap();

        // Read two frames: the announce_subject event and the
        // load_response. The order is constrained by the plugin code:
        // the announcement happens before load returns.
        let first = read_frame_json(&mut client_r).await.unwrap();
        let second = read_frame_json(&mut client_r).await.unwrap();

        // Whichever order they arrive in, we should see one of each.
        let (announce, load_resp) = match (&first, &second) {
            (WireFrame::AnnounceSubject { .. }, WireFrame::LoadResponse { .. }) => {
                (&first, &second)
            }
            (WireFrame::LoadResponse { .. }, WireFrame::AnnounceSubject { .. }) => {
                (&second, &first)
            }
            _ => panic!(
                "expected AnnounceSubject + LoadResponse, got {first:?} and {second:?}"
            ),
        };

        match announce {
            WireFrame::AnnounceSubject {
                plugin,
                announcement: a,
                ..
            } => {
                assert_eq!(plugin, "org.test.x");
                assert_eq!(a.subject_type, "track");
            }
            _ => unreachable!(),
        }
        match load_resp {
            WireFrame::LoadResponse { cid, .. } => assert_eq!(*cid, 1),
            _ => unreachable!(),
        }

        drop(client_w);
        drop(client_r);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn json_config_converts_to_toml_table() {
        let v = serde_json::json!({
            "key": "value",
            "n": 42,
            "f": 3.14,
            "b": true,
            "nested": {
                "inner": "x"
            },
            "list": [1, 2, 3]
        });
        let t = json_value_to_toml_table(v).unwrap();
        assert_eq!(t.get("key").unwrap().as_str(), Some("value"));
        assert_eq!(t.get("n").unwrap().as_integer(), Some(42));
        assert_eq!(t.get("f").unwrap().as_float(), Some(3.14));
        assert_eq!(t.get("b").unwrap().as_bool(), Some(true));
        assert!(t.get("nested").unwrap().as_table().is_some());
        assert!(t.get("list").unwrap().as_array().is_some());
    }

    #[tokio::test]
    async fn json_null_in_config_is_rejected() {
        let v = serde_json::json!({"x": null});
        let err = json_value_to_toml_table(v).unwrap_err();
        assert!(err.contains("null"));
    }

    #[tokio::test]
    async fn non_object_config_is_rejected() {
        let v = serde_json::json!(["not", "an", "object"]);
        let err = json_value_to_toml_table(v).unwrap_err();
        assert!(err.contains("object"));
    }
}
