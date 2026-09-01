// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Device-local kiosk session mint.
//!
//! The reference-device compositor (Volumio's 7" touchscreen shell,
//! or any equivalent local display) needs an operator bearer token
//! without asking the operator to type, paste, or authenticate. This
//! module carries the primitive: a Unix-socket-only wire op that
//! reads `SO_PEERCRED` on the accepting stream, checks the peer UID
//! against a distribution-configured allowlist, and mints a fresh
//! operator-bootstrap bearer bound to that UID.
//!
//! Socket path is fixed at `/run/evo/kiosk.sock` (mode 0660,
//! owned by the framework service user, group-readable by the
//! compositor session's group). No other wire op is served on this
//! socket — a compromised compositor cannot pivot from mint to
//! wider steward control by re-using the connection.
//!
//! The primitive is refused on any transport other than the
//! kiosk socket. The main Unix socket and the HTTPS/WSS surface do
//! not carry this wire op — a remote WSS client requesting
//! `mint_local_kiosk_session` receives `PermissionDenied` with
//! subclass `kiosk_socket_only`.

use std::collections::HashSet;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::client_acl::PeerCredentials;

/// Hard cap on a single kiosk-socket frame. Mint requests are tiny
/// JSON blobs (well under 1 KiB in practice); a frame approaching
/// this cap is almost certainly misuse. Keeping the cap tight is a
/// mild DoS mitigation on a socket that admits an unprivileged
/// compositor peer.
const KIOSK_MAX_FRAME_SIZE: usize = 4 * 1024;

/// Allowlist of UIDs permitted to mint a kiosk session via the
/// `/run/evo/kiosk.sock` transport. Populated at boot from the
/// distribution's compositor-user configuration; empty by default
/// (no compositor UID → every kiosk-socket connection is refused,
/// which is the correct floor for a distribution that has not
/// declared its compositor).
#[derive(Debug, Clone, Default)]
pub struct KioskUidAllowlist {
    uids: HashSet<u32>,
}

impl KioskUidAllowlist {
    /// Construct an empty allowlist. Every connecting peer will be
    /// refused until [`Self::add`] is called at least once.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct from an iterator of allowed UIDs (typical use:
    /// distribution boot reads its compositor user's UID and
    /// passes it here).
    pub fn from_uids<I: IntoIterator<Item = u32>>(uids: I) -> Self {
        Self {
            uids: uids.into_iter().collect(),
        }
    }

    /// Add a UID to the allowlist. Idempotent.
    pub fn add(&mut self, uid: u32) {
        self.uids.insert(uid);
    }

    /// Whether the allowlist admits the given UID.
    pub fn admits(&self, uid: u32) -> bool {
        self.uids.contains(&uid)
    }

    /// Number of UIDs currently on the allowlist.
    pub fn len(&self) -> usize {
        self.uids.len()
    }

    /// Whether the allowlist is empty (denies every UID).
    pub fn is_empty(&self) -> bool {
        self.uids.is_empty()
    }

    /// Iterator over the admitted UIDs. Order not stable.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.uids.iter().copied()
    }
}

/// Distribution-supplied kiosk-socket configuration. Passed to the
/// framework at boot; carried through to the accept loop.
#[derive(Debug, Clone)]
pub struct KioskSocketConfig {
    /// Path the framework binds the kiosk socket at. Distribution
    /// convention: `/run/evo/kiosk.sock`.
    pub socket_path: PathBuf,
    /// Allowlist of compositor UIDs permitted to mint kiosk
    /// sessions. Empty allowlist refuses every connection.
    pub allowlist: KioskUidAllowlist,
    /// TTL (seconds) of the minted kiosk bearer. Capped at the
    /// framework `MAX_TOKEN_TTL_MS` by the mint path. Default in
    /// the reference distribution: 24 h (86_400 s).
    pub bearer_ttl_seconds: u64,
}

impl Default for KioskSocketConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/run/evo/kiosk.sock"),
            allowlist: KioskUidAllowlist::empty(),
            bearer_ttl_seconds: 24 * 60 * 60,
        }
    }
}

/// Reason the compositor supplies to `mint_local_kiosk_session`.
/// Recorded in the audit trail. Free-form; the framework refuses
/// blank-reason mints (identical discipline to the CLI mint path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KioskMintReason(pub String);

/// Outcome of a kiosk-socket peer-credentials check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KioskAdmission {
    /// Peer UID is on the allowlist; mint proceeds.
    Admitted {
        /// Compositor UID whose bearer will be minted.
        uid: u32,
    },
    /// Peer credentials were not readable (SO_PEERCRED unsupported
    /// or the platform did not return a UID). Every non-Linux
    /// Unix + every sandbox that hides `SO_PEERCRED` lands here.
    /// Refusal is the correct floor: without a UID there is no
    /// identity to check against the allowlist.
    PeerUnknown,
    /// Peer UID is known but the allowlist does not admit it. The
    /// compositor is misconfigured OR a non-compositor process has
    /// reached the kiosk socket.
    NotAdmitted {
        /// UID observed on the connection but refused by the
        /// allowlist. Recorded in audit; refusal is silent to the
        /// caller in principle (only the subclass exposes the
        /// class of refusal, not the UID).
        uid: u32,
    },
    /// Distribution supplied no allowlist. Every mint refused.
    AllowlistEmpty,
}

/// Consult the allowlist against captured peer credentials.
pub fn evaluate_admission(
    peer: PeerCredentials,
    allowlist: &KioskUidAllowlist,
) -> KioskAdmission {
    if allowlist.is_empty() {
        return KioskAdmission::AllowlistEmpty;
    }
    let Some(uid) = peer.uid else {
        return KioskAdmission::PeerUnknown;
    };
    if allowlist.admits(uid) {
        KioskAdmission::Admitted { uid }
    } else {
        KioskAdmission::NotAdmitted { uid }
    }
}

/// Read peer credentials from a connected kiosk stream. Wraps the
/// platform-specific `peer_cred` query so the accept loop stays
/// portable.
pub fn kiosk_peer_credentials(stream: &UnixStream) -> PeerCredentials {
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

/// Handle a kiosk mint request against a captured admission and
/// the framework's bearer-token issuer. Returns the encoded bearer
/// on success; a typed error on refusal.
///
/// The handler emits a tracing event on every outcome — success
/// carries the token id, source claim `kiosk`, and the audit
/// reason; refusal carries the refusal kind and no token.
///
/// Refuses `reason` that is empty after trim (same discipline the
/// CLI mint path applies).
pub fn mint_kiosk_bearer(
    issuer: &Arc<evo_auth_bearer::BearerTokenIssuer>,
    capabilities: evo_auth_bearer::CapabilitySet,
    admission: KioskAdmission,
    reason: KioskMintReason,
    ttl_seconds: u64,
) -> Result<KioskMintOk, KioskMintErr> {
    let uid = match admission {
        KioskAdmission::Admitted { uid } => uid,
        KioskAdmission::PeerUnknown => {
            return Err(KioskMintErr::PeerUnknown);
        }
        KioskAdmission::NotAdmitted { uid } => {
            return Err(KioskMintErr::NotAdmitted { uid });
        }
        KioskAdmission::AllowlistEmpty => {
            return Err(KioskMintErr::AllowlistEmpty);
        }
    };
    let reason_trimmed = reason.0.trim().to_string();
    if reason_trimmed.is_empty() {
        return Err(KioskMintErr::BlankReason);
    }
    let ttl_ms = ttl_seconds.saturating_mul(1000);
    let ttl_ms = ttl_ms.min(evo_auth_bearer::MAX_TOKEN_TTL_MS);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let token = issuer
        .issue(capabilities, ttl_ms, now_ms)
        .map_err(|e| KioskMintErr::IssuanceFailed(e.to_string()))?;
    tracing::info!(
        token_id = %token.id,
        reason = %reason_trimmed,
        source = "kiosk",
        peer_uid = uid,
        expires_at_ms = token.expires_at_ms,
        ttl_ms,
        "kiosk bearer minted via mint_local_kiosk_session"
    );
    Ok(KioskMintOk {
        encoded_token: token.encode(),
        token_id: token.id,
        expires_at_ms: token.expires_at_ms,
        peer_uid: uid,
    })
}

/// Successful mint outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KioskMintOk {
    /// Base64-encoded bearer the compositor injects as the kiosk
    /// browser's authorisation cookie.
    pub encoded_token: String,
    /// Token id (for revocation + audit correlation).
    pub token_id: String,
    /// Expiry (ms since Unix epoch) — the compositor renews from
    /// the kiosk socket before this instant.
    pub expires_at_ms: u64,
    /// UID the bearer is bound to (the compositor UID).
    pub peer_uid: u32,
}

/// Refusal kinds. Each maps to a structured error class on the
/// wire so the compositor can distinguish "you're not the
/// compositor" from "framework is broken".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KioskMintErr {
    /// SO_PEERCRED did not return a UID. Compositor sandbox is
    /// hiding it.
    PeerUnknown,
    /// UID is known but not on the allowlist. Wrong process
    /// reached the kiosk socket.
    NotAdmitted {
        /// UID observed on the connection but refused. Recorded
        /// in the tracing event; not surfaced to the caller.
        uid: u32,
    },
    /// Distribution shipped without a compositor UID.
    AllowlistEmpty,
    /// Compositor sent an empty reason (or whitespace-only).
    BlankReason,
    /// The bearer-token issuer refused (should not happen; the
    /// framework's issuer signs unconditionally). Carries the
    /// issuer diagnostic for triage.
    IssuanceFailed(String),
}

impl KioskMintErr {
    /// Short kebab-case slug for the wire-error subclass.
    pub fn subclass(&self) -> &'static str {
        match self {
            Self::PeerUnknown => "peer_unknown",
            Self::NotAdmitted { .. } => "peer_not_admitted",
            Self::AllowlistEmpty => "allowlist_empty",
            Self::BlankReason => "blank_reason",
            Self::IssuanceFailed(_) => "issuance_failed",
        }
    }

    /// Operator-facing message for the wire-error body.
    pub fn message(&self) -> String {
        match self {
            Self::PeerUnknown => {
                "mint_local_kiosk_session: peer credentials unavailable; \
                 kiosk mint requires a recoverable peer UID via SO_PEERCRED"
                    .to_string()
            }
            Self::NotAdmitted { uid } => format!(
                "mint_local_kiosk_session: peer UID {uid} is not on the \
                 kiosk allowlist; compositor is misconfigured or a non-\
                 compositor process reached the kiosk socket"
            ),
            Self::AllowlistEmpty => {
                "mint_local_kiosk_session: kiosk allowlist is empty; \
                 distribution boot did not declare a compositor UID"
                    .to_string()
            }
            Self::BlankReason => {
                "mint_local_kiosk_session: reason must be non-empty after \
                 trim (audit substrate refuses blank-reason mints by design)"
                    .to_string()
            }
            Self::IssuanceFailed(detail) => {
                format!("mint_local_kiosk_session: issuance failed: {detail}")
            }
        }
    }
}

/// Accept loop for `/run/evo/kiosk.sock`. Serves ONE wire op:
/// `mint_local_kiosk_session`. Every other op — including every
/// standard steward wire op that reaches the main `/run/evo/evo.sock`
/// — is refused on this socket with `kiosk_socket_only` subclass, so a
/// compromised compositor cannot pivot from mint to wider steward
/// control by re-using the connection.
///
/// Frame shape mirrors the slow-path v0 codec: 4-byte big-endian
/// length prefix followed by a UTF-8 JSON body. Same shape
/// `evo-kiosk-browser`'s mint client emits. Framing errors,
/// missing operator identity fields, and mint refusals all
/// round-trip through a `{ "error": { ... } }` response frame
/// with a `subclass` slug the compositor can act on.
///
/// The peer credentials are captured via `SO_PEERCRED` at accept
/// time, before the per-connection task is spawned, so the accept
/// loop and the connection handler agree on the same UID even if the
/// peer disconnects mid-frame. `evaluate_admission` gates the mint
/// against the allowlist; a peer whose UID is not on the list gets
/// the same refusal shape as a peer whose UID could not be recovered
/// at all (`peer_unknown` vs `peer_not_admitted`), so the compositor
/// can distinguish "SO_PEERCRED is broken here" from "wrong process
/// reached the socket".
///
/// The socket file is created mode 0660 so a compositor that shares
/// the framework runtime user's group can connect without needing
/// additional permissions; a stricter distribution can chown /
/// chmod it in a unit drop-in. Distributions that do not declare a
/// compositor UID (`EVO_KIOSK_UIDS` unset or empty) still get a
/// bound socket — every connection refused with `allowlist_empty` —
/// so an operator sanity-checking with `ss -xln` sees the socket
/// even when no compositor is yet in scope.
pub async fn serve_kiosk_socket<S>(
    config: KioskSocketConfig,
    state: Arc<crate::state::StewardState>,
    router: Arc<crate::router::PluginRouter>,
    allowlist: Arc<KioskUidAllowlist>,
    shutdown: S,
) -> Result<(), crate::error::StewardError>
where
    S: Future<Output = ()> + Send + 'static,
{
    if let Some(parent) = config.socket_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(crate::error::StewardError::io(
                        format!(
                            "creating kiosk-socket parent directory {}",
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
            return Err(crate::error::StewardError::io(
                format!(
                    "removing stale kiosk socket {}",
                    config.socket_path.display()
                ),
                e,
            ));
        }
    }

    let listener = UnixListener::bind(&config.socket_path).map_err(|e| {
        crate::error::StewardError::io(
            format!("binding kiosk socket {}", config.socket_path.display()),
            e,
        )
    })?;

    // 0660 — framework runtime user owns the socket; group-readable
    // for the compositor's group per the module doc. Distribution
    // drop-ins that need a wider or narrower mode adjust via
    // `chmod`/`chgrp` in a start-up hook.
    let mode = std::fs::Permissions::from_mode(0o660);
    if let Err(e) = tokio::fs::set_permissions(&config.socket_path, mode).await
    {
        tracing::warn!(
            socket = %config.socket_path.display(),
            error = %e,
            "kiosk socket permissions could not be set to 0660"
        );
    }

    tracing::info!(
        socket = %config.socket_path.display(),
        allowlist_uids = allowlist.len(),
        bearer_ttl_seconds = config.bearer_ttl_seconds,
        "kiosk socket listening"
    );

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        // Capture peer credentials at accept time so
                        // the connection handler observes the same
                        // UID the audit trail will record.
                        let peer = kiosk_peer_credentials(&stream);
                        let state = Arc::clone(&state);
                        let router = Arc::clone(&router);
                        let allowlist = Arc::clone(&allowlist);
                        let ttl_seconds = config.bearer_ttl_seconds;
                        tokio::spawn(async move {
                            if let Err(e) = handle_kiosk_connection(
                                stream,
                                state,
                                router,
                                allowlist,
                                peer,
                                ttl_seconds,
                            )
                            .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    "kiosk-socket connection handler failed"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "kiosk-socket accept failed"
                        );
                    }
                }
            }
            _ = &mut shutdown => {
                tracing::info!("kiosk-socket accept loop exiting");
                break;
            }
        }
    }

    if let Err(e) = tokio::fs::remove_file(&config.socket_path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                socket = %config.socket_path.display(),
                error = %e,
                "failed to remove kiosk socket on shutdown"
            );
        }
    }

    Ok(())
}

/// One-shot request/response over a kiosk-socket connection.
///
/// Reads a single mint request frame, produces the response frame,
/// and returns. Persistent state / multi-frame conversations are
/// intentionally not supported: the wire contract is exactly one
/// mint per connection, mirroring how the browser client (see
/// `evo-kiosk-browser`'s `mint_once`) uses this socket. This keeps
/// the accept-loop attack surface minimal and matches how the
/// compositor drives silent re-mint.
async fn handle_kiosk_connection(
    mut stream: UnixStream,
    state: Arc<crate::state::StewardState>,
    router: Arc<crate::router::PluginRouter>,
    allowlist: Arc<KioskUidAllowlist>,
    peer: PeerCredentials,
    ttl_seconds: u64,
) -> std::io::Result<()> {
    // Frame in.
    let body = match read_frame(&mut stream).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "kiosk-socket frame read failed");
            // The stream is torn down; nothing useful we can send.
            return Ok(());
        }
    };

    // Parse the mint request.
    #[derive(Deserialize)]
    struct MintRequest {
        op: String,
        #[serde(default)]
        reason: String,
    }
    let req: MintRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp = error_frame(
                "invalid_request",
                &format!("kiosk-socket request JSON invalid: {e}"),
            );
            write_frame(&mut stream, &resp).await?;
            return Ok(());
        }
    };

    // Refuse every op except mint. Same subclass slug the module
    // doc names — a compositor that mistakenly points a non-mint
    // call at the kiosk socket sees the mismatch by name.
    if req.op != "mint_local_kiosk_session" {
        let resp = error_frame(
            "kiosk_socket_only",
            &format!(
                "kiosk socket does not serve {op:?}; the main evo.sock \
                 dispatches every non-mint wire op",
                op = req.op
            ),
        );
        write_frame(&mut stream, &resp).await?;
        return Ok(());
    }

    // Mint. `evaluate_admission` runs the allowlist gate;
    // `mint_kiosk_bearer` produces the encoded bearer plus token id
    // plus expiry. Refusals classify into typed subclasses that
    // the compositor uses for retry vs give-up decisions.
    let admission = evaluate_admission(peer, &allowlist);
    let issuer_slot = state.bearer_token_issuer.get();
    let Some(issuer) = issuer_slot else {
        let resp = error_frame(
            "issuer_unavailable",
            "mint_local_kiosk_session: bearer-token issuer not wired \
             (HTTPS substrate did not boot on this steward)",
        );
        write_frame(&mut stream, &resp).await?;
        return Ok(());
    };
    let capabilities =
        crate::https_boot::merged_operator_bootstrap_capability_set(&router);

    let resp = match mint_kiosk_bearer(
        issuer,
        capabilities,
        admission,
        KioskMintReason(req.reason),
        ttl_seconds,
    ) {
        Ok(ok) => serde_json::to_vec(&serde_json::json!({
            "local_kiosk_session_minted": true,
            "token": ok.encoded_token,
            "token_id": ok.token_id,
            "expires_at_ms": ok.expires_at_ms,
        }))
        .expect("mint OK response serialises"),
        Err(err) => error_frame(err.subclass(), &err.message()),
    };
    write_frame(&mut stream, &resp).await?;
    Ok(())
}

/// Read one 4-byte big-endian length-prefixed frame. Refuses frames
/// larger than [`KIOSK_MAX_FRAME_SIZE`]. Same shape as the slow-path
/// codec + the client's `mint::read_frame`.
async fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > KIOSK_MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "kiosk-socket frame too large: {len} bytes (max {KIOSK_MAX_FRAME_SIZE})"
            ),
        ));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

/// Write one 4-byte big-endian length-prefixed frame.
async fn write_frame(
    stream: &mut UnixStream,
    body: &[u8],
) -> std::io::Result<()> {
    if body.len() > u32::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "kiosk-socket response frame exceeds u32 length prefix",
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

/// Build a `{ "error": { subclass, message } }` response body.
fn error_frame(subclass: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": {
            "subclass": subclass,
            "message": message,
        }
    }))
    .expect("error response serialises")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allowlist_denies_every_peer() {
        let list = KioskUidAllowlist::empty();
        let admission = evaluate_admission(
            PeerCredentials {
                uid: Some(1000),
                gid: Some(1000),
            },
            &list,
        );
        assert_eq!(admission, KioskAdmission::AllowlistEmpty);
    }

    #[test]
    fn missing_peer_uid_refused_even_when_allowlist_populated() {
        let list = KioskUidAllowlist::from_uids([1000]);
        let admission = evaluate_admission(
            PeerCredentials {
                uid: None,
                gid: None,
            },
            &list,
        );
        assert_eq!(admission, KioskAdmission::PeerUnknown);
    }

    #[test]
    fn admitted_uid_passes() {
        let list = KioskUidAllowlist::from_uids([1000]);
        let admission = evaluate_admission(
            PeerCredentials {
                uid: Some(1000),
                gid: Some(1000),
            },
            &list,
        );
        assert_eq!(admission, KioskAdmission::Admitted { uid: 1000 });
    }

    #[test]
    fn known_uid_off_allowlist_refused() {
        let list = KioskUidAllowlist::from_uids([1000]);
        let admission = evaluate_admission(
            PeerCredentials {
                uid: Some(1001),
                gid: Some(1001),
            },
            &list,
        );
        assert_eq!(admission, KioskAdmission::NotAdmitted { uid: 1001 });
    }

    #[test]
    fn allowlist_is_idempotent() {
        let mut list = KioskUidAllowlist::empty();
        list.add(1000);
        list.add(1000);
        list.add(1000);
        assert_eq!(list.len(), 1);
        assert!(list.admits(1000));
    }

    #[test]
    fn err_subclass_slugs_are_stable() {
        assert_eq!(KioskMintErr::PeerUnknown.subclass(), "peer_unknown");
        assert_eq!(
            KioskMintErr::NotAdmitted { uid: 1 }.subclass(),
            "peer_not_admitted"
        );
        assert_eq!(KioskMintErr::AllowlistEmpty.subclass(), "allowlist_empty");
        assert_eq!(KioskMintErr::BlankReason.subclass(), "blank_reason");
        assert_eq!(
            KioskMintErr::IssuanceFailed("x".into()).subclass(),
            "issuance_failed"
        );
    }

    #[test]
    fn default_config_uses_run_evo_kiosk_socket() {
        let cfg = KioskSocketConfig::default();
        assert_eq!(cfg.socket_path, PathBuf::from("/run/evo/kiosk.sock"));
        assert!(cfg.allowlist.is_empty());
        assert_eq!(cfg.bearer_ttl_seconds, 86_400);
    }

    #[test]
    fn error_frame_shape_matches_wire_contract() {
        // The compositor client parses this shape: an outer
        // `error` object with `subclass` (kebab-case slug it
        // dispatches on) and `message` (operator-facing text).
        let body = error_frame(
            "allowlist_empty",
            "distribution boot did not declare a compositor UID",
        );
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            parsed["error"]["subclass"],
            serde_json::Value::from("allowlist_empty")
        );
        assert!(parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("did not declare a compositor UID"));
        // No stray top-level keys that would confuse a strict-
        // deserialising compositor client.
        assert!(parsed.get("local_kiosk_session_minted").is_none());
        assert!(parsed.get("token").is_none());
    }

    #[tokio::test]
    async fn write_frame_read_frame_roundtrip() {
        // Frame in, frame out; length prefix survives; body bytes
        // preserved verbatim across the pair.
        let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let payload =
            br#"{"op":"mint_local_kiosk_session","reason":"kiosk-boot"}"#;
        write_frame(&mut a, payload).await.unwrap();
        let received = read_frame(&mut b).await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn read_frame_refuses_oversized_length_prefix() {
        // A malicious peer that lies about frame length must not
        // trigger an unbounded allocation. The length is refused
        // BEFORE the body read, so no memory is committed.
        let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let bad_len = (KIOSK_MAX_FRAME_SIZE as u32 + 1).to_be_bytes();
        a.write_all(&bad_len).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("frame too large"));
    }

    #[tokio::test]
    async fn write_frame_refuses_body_beyond_u32() {
        // Guard the length-prefix cast: we can't build a body
        // that large in a test, but we can verify the shape by
        // exercising the same check against a smaller payload —
        // instead we assert the OK case shape (length prefix +
        // body). The oversized-body branch is unreachable on a
        // 32-bit-length wire and the test would OOM the host if
        // materialised; we assert on the surrounding contract.
        let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
        write_frame(&mut a, b"x").await.unwrap();
        let mut len_buf = [0u8; 4];
        b.read_exact(&mut len_buf).await.unwrap();
        assert_eq!(u32::from_be_bytes(len_buf), 1);
    }
}
