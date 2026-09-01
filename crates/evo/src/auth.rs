// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Step-up authentication primitive for privileged operations.
//!
//! Defines the typed contract under which the operator-facing UI and
//! the operator CLI execute high-impact actions (plugin lifecycle,
//! channel selection, update execution, log-level mutation,
//! diagnostics export, SSH state). Two collaborating shapes:
//!
//! - [`AuthService`]: the pluggable verification hook implemented by
//!   the vendor distribution. The framework does not embed any
//!   concrete authenticator; it defines the typed call (`verify`),
//!   the error taxonomy, and the audit-emission shape. Vendor
//!   distributions bind this to PAM, a vendor IdP, or any equivalent
//!   credential authority. The framework default
//!   [`NoAuthService`] denies every verification attempt — a
//!   distribution that ships without an `AuthService` implementation
//!   has no way to elevate to privileged operations, which is the
//!   correct floor.
//!
//! - [`AuthSessionStore`]: in-memory map of active privileged
//!   sessions. On a successful verification call, the store issues a
//!   short-lived token bound to the verifying peer's UID; the token
//!   appears on every subsequent privileged ClientRequest as proof
//!   the operator stepped up. Tokens carry a TTL bounded above by a
//!   framework-set ceiling — operators may configure shorter TTLs
//!   but cannot extend beyond the ceiling. Tokens are never
//!   persisted; a steward restart invalidates every active session.
//!
//! Tokens are URL-safe base64 of 32 cryptographically random bytes.
//! The store rejects validation calls when the token is unknown,
//! expired, or the presenting peer's UID does not match the UID the
//! token was bound to at issuance — this prevents a token leaked
//! through a sibling connection on the same socket from being used
//! by an unrelated process.
//!
//! Audit ledger integration lives in [`crate::ledger`]; every
//! verification attempt and every privileged-operation execution
//! emits a typed entry independent of the wire layer. The wire layer
//! adds three ClientRequest variants — `step_up_auth_verify`,
//! `step_up_auth_revoke`, `step_up_auth_status` — wired into the
//! existing client-API shape.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Framework-set ceiling for privileged-session TTLs. An operator
/// configuration may request a shorter TTL but cannot extend beyond
/// this value. Locked at compile time so a misconfigured deployment
/// cannot extend privileged sessions arbitrarily.
pub const STEP_UP_TOKEN_TTL_CEILING: Duration = Duration::from_secs(15 * 60);

/// Default TTL applied when neither the verification request nor
/// the operator config supplies an explicit TTL.
pub const STEP_UP_TOKEN_TTL_DEFAULT: Duration = Duration::from_secs(5 * 60);

/// Length, in bytes, of the random material backing each issued
/// token. Encoded as URL-safe base64 (no padding) when serialised.
const TOKEN_RANDOM_BYTES: usize = 32;

/// Successful verification result. Carries the principal the
/// implementation authenticated; the framework binds the issued
/// token to this principal and to the verifying peer's UID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedPrincipal {
    /// Username (or equivalent stable identifier) the implementation
    /// authenticated. Recorded in audit entries; opaque to the
    /// framework.
    pub username: String,
    /// OS UID the principal corresponds to, if the implementation
    /// can report one. `None` for implementations that authenticate
    /// against a non-OS authority.
    pub uid: Option<u32>,
}

/// Reasons a verification call may fail. The implementation maps
/// its native error vocabulary onto this enum so the framework can
/// classify outcomes uniformly in audit and error responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthVerificationError {
    /// The username and secret combination did not authenticate.
    /// Implementations MUST NOT differentiate "username unknown"
    /// from "wrong secret" in this variant — both collapse here so
    /// timing and surface signal stay uniform.
    InvalidCredentials,
    /// The principal authenticated but is not permitted to perform
    /// privileged operations against this steward (e.g. not a
    /// member of the operators group on this device).
    UserNotPermitted,
    /// The verification backend is fundamentally unavailable — no
    /// implementation configured, PAM stack down, IdP network
    /// failure at the transport layer. Distinct from
    /// [`Self::BackendReadFailed`] (backend implementation is in
    /// place but couldn't read the stored credential) and from
    /// [`Self::BackendVerifyError`] (backend read the credential
    /// but the verification primitive rejected it). The reason
    /// string is recorded in audit but not exposed in the operator
    /// error response — operators see a generic "step-up
    /// unavailable" message so backend topology does not leak.
    BackendUnavailable {
        /// Implementation-side reason. Recorded in audit only.
        reason: String,
    },
    /// The verification backend is in place but could not read the
    /// stored credential material. Typical case: shadow-file
    /// backend where `/etc/shadow` is unreadable (framework
    /// runtime user not in the `shadow` group) or the row for the
    /// runtime user is absent. Distribution-setup defect, not an
    /// operator-fixable credential problem. Distinct from
    /// [`Self::BackendUnavailable`] so the operator surface can
    /// tell "distribution installed the framework wrong" apart
    /// from "no step-up backend configured at all".
    BackendReadFailed {
        /// Implementation-side reason. Recorded in audit only.
        reason: String,
    },
    /// The verification backend read the stored credential but the
    /// verification primitive rejected the specific stored value —
    /// unsupported hash format, malformed hash, libcrypt internal
    /// error. Distinct from [`Self::InvalidCredentials`] (which
    /// means "hash format understood; wrong password") and from
    /// [`Self::BackendUnavailable`] / [`Self::BackendReadFailed`]
    /// (backend-level problems). Recovery is a password rotation
    /// via `set_kiosk_password` (or the OS-native equivalent)
    /// that rewrites the row in a format the backend can consume.
    BackendVerifyError {
        /// Implementation-side reason. Recorded in audit only.
        reason: String,
    },
}

/// Pluggable step-up verification hook. The framework calls
/// [`AuthService::verify`] on every step-up ClientRequest;
/// implementations bind this to PAM, a vendor IdP, or equivalent.
///
/// Implementations MUST treat `secret` as sensitive: zero the slice
/// after use, never log, never persist, never return it on a
/// debug formatter. The framework's default [`NoAuthService`]
/// satisfies this trivially (it does not consume the secret at all).
///
/// The trait is sync; long-running verifications (network IdP,
/// remote LDAP) should spawn a blocking task internally so the
/// async server runtime is not blocked. The verify call has no
/// budget at the framework layer — the wire layer enforces an
/// overall request timeout that bounds verification end-to-end.
pub trait AuthService: Send + Sync + std::fmt::Debug {
    /// Authenticate `username` + `secret`. Returns the verified
    /// principal on success; otherwise an [`AuthVerificationError`]
    /// in the framework's classified taxonomy.
    fn verify(
        &self,
        username: &str,
        secret: &[u8],
    ) -> Result<VerifiedPrincipal, AuthVerificationError>;

    /// Stable identifier for the implementation, recorded on every
    /// audit entry so external auditors can correlate verification
    /// outcomes with the backend that produced them. Returned value
    /// SHOULD be a short ASCII identifier
    /// (`"pam"`, `"vendor-idp"`, `"none"`).
    fn implementation_name(&self) -> &'static str;

    /// Refresh the implementation's in-memory state after the
    /// framework rewrote the on-disk backing (e.g.,
    /// `set_kiosk_password` wrote a new argon2id hash to the
    /// shared-secret file). Default is a no-op — an
    /// implementation whose state is not file-backed (PAM,
    /// vendor IdP) has nothing to refresh. Implementations
    /// whose state IS file-backed (like
    /// [`SharedSecretAuthService`]) override to re-parse the
    /// file and swap in a fresh set atomically.
    ///
    /// Called from the `set_kiosk_password` handler after the
    /// file rewrite succeeds. On error, the previous state
    /// stays in place — a failed refresh does not corrupt
    /// verification.
    fn refresh_after_secret_write(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Framework default: deny every verification attempt with
/// `BackendUnavailable`. A distribution that ships without
/// configuring an [`AuthService`] cannot elevate to privileged
/// operations — which is the correct floor (no implicit
/// authentication, no implicit privilege).
///
/// Vendor distributions plug in a real implementation at the wiring
/// layer alongside [`crate::ledger::CryptographicServices`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAuthService;

impl AuthService for NoAuthService {
    fn verify(
        &self,
        _username: &str,
        _secret: &[u8],
    ) -> Result<VerifiedPrincipal, AuthVerificationError> {
        Err(AuthVerificationError::BackendUnavailable {
            reason: "no AuthService configured; vendor distribution must \
                     plug in a verification hook to enable privileged \
                     operations"
                .into(),
        })
    }

    fn implementation_name(&self) -> &'static str {
        "none"
    }
}

/// Framework-default [`AuthService`] backend: argon2id-hashed
/// per-user secrets loaded from a TOML file at boot.
///
/// Vendor distributions that ship without their own PAM / LDAP /
/// WebAuthn integration can wire this directly; vendors that bring
/// a stronger auth surface plug in their own [`AuthService`]
/// implementation and skip this default entirely. The trait is the
/// contract; this struct is one realisation.
///
/// # File format
///
/// ```toml
/// schema_version = 1
///
/// [[users]]
/// username = "operator"
/// # argon2id hash produced by any compliant tool:
/// #   argon2 mySalt -id -e <<<'mySecret'
/// # or via the password-hash crate's PasswordHasher.
/// password_hash = "$argon2id$v=19$m=65536,t=3,p=4$..."
/// ```
///
/// Per-user `password_hash` strings carry the full PHC-format
/// argon2id encoded hash including salt, m / t / p parameters,
/// and the digest. Hash validation is delegated to the
/// [`argon2::PasswordVerifier`] contract, which performs the
/// argon2id derivation in constant time relative to the candidate
/// secret length.
///
/// The verifier does not differentiate "username unknown" from
/// "wrong secret" on the wire — both collapse to
/// [`AuthVerificationError::InvalidCredentials`], preserving the
/// uniform timing / surface the trait contract requires.
pub struct SharedSecretAuthService {
    /// Username → PHC-formatted argon2id password hash. Guarded
    /// by an [`RwLock`] so [`Self::refresh_from_disk`] can swap
    /// in a freshly-loaded set atomically after
    /// `set_kiosk_password` rewrites the on-disk file — the
    /// in-memory state stays consistent with disk without a
    /// steward restart.
    users: std::sync::RwLock<HashMap<String, String>>,
    /// Path the service was loaded from, remembered so
    /// [`Self::refresh_from_disk`] can re-read the same file
    /// without a caller-supplied path. `None` when the service
    /// was constructed via [`Self::from_map`] (tests / in-memory
    /// scenarios) — refresh in that state is a no-op that
    /// returns `Ok(())`.
    source_path: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for SharedSecretAuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.users.read().map(|u| u.len()).unwrap_or(usize::MAX);
        f.debug_struct("SharedSecretAuthService")
            .field("user_count", &count)
            .field("source_path", &self.source_path)
            .finish_non_exhaustive()
    }
}

/// Errors raised when loading a [`SharedSecretAuthService`]
/// secret-file.
#[derive(Debug, thiserror::Error)]
pub enum SharedSecretLoadError {
    /// The file at the configured path could not be read.
    #[error("read {path}: {source}")]
    Read {
        /// File path the loader attempted.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file content did not parse as TOML in the expected
    /// shape.
    #[error("parse {path}: {detail}")]
    Parse {
        /// File path the loader attempted.
        path: String,
        /// Toml or deserialisation diagnostic.
        detail: String,
    },
    /// `schema_version` mismatch — the loader refuses to interpret
    /// a file authored against a different schema rather than
    /// silently best-effort.
    #[error(
        "unsupported schema_version {found} in {path}; framework expects 1"
    )]
    UnsupportedSchemaVersion {
        /// File path the loader attempted.
        path: String,
        /// Schema version found in the file.
        found: u32,
    },
    /// A `password_hash` field was not a valid PHC-format argon2id
    /// hash. Surfaced at load time so a malformed hash never reaches
    /// a verification call.
    #[error("invalid password_hash for user {username:?} in {path}: {detail}")]
    InvalidHash {
        /// File path the loader attempted.
        path: String,
        /// Username whose hash failed to parse.
        username: String,
        /// password-hash crate diagnostic.
        detail: String,
    },
    /// The file declared the same `username` twice. Refused at load
    /// time so credential resolution stays unambiguous.
    #[error("duplicate username {username:?} in {path}")]
    DuplicateUsername {
        /// File path the loader attempted.
        path: String,
        /// Username repeated in the file.
        username: String,
    },
}

#[derive(Debug, Deserialize)]
struct SharedSecretFile {
    schema_version: u32,
    #[serde(default)]
    users: Vec<SharedSecretUser>,
}

#[derive(Debug, Deserialize)]
struct SharedSecretUser {
    username: String,
    password_hash: String,
}

impl SharedSecretAuthService {
    /// Construct from an explicit username → PHC-hash map.
    /// Intended for tests; production callers use
    /// [`Self::load_from_file`]. [`Self::refresh_from_disk`]
    /// on a service built via this constructor is a no-op
    /// (there is no source file to re-read).
    pub fn from_map(users: HashMap<String, String>) -> Self {
        Self {
            users: std::sync::RwLock::new(users),
            source_path: None,
        }
    }

    /// Parse a secrets-file body into a `username → PHC-hash`
    /// map. Extracted from [`Self::load_from_file`] so both the
    /// initial-load path and the runtime refresh path share the
    /// same parse + validate discipline.
    fn parse_document(
        path: &std::path::Path,
        text: &str,
    ) -> Result<HashMap<String, String>, SharedSecretLoadError> {
        let parsed: SharedSecretFile =
            toml::from_str(text).map_err(|e| SharedSecretLoadError::Parse {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        if parsed.schema_version != 1 {
            return Err(SharedSecretLoadError::UnsupportedSchemaVersion {
                path: path.display().to_string(),
                found: parsed.schema_version,
            });
        }
        let mut users = HashMap::with_capacity(parsed.users.len());
        for user in parsed.users {
            argon2::password_hash::PasswordHash::new(&user.password_hash)
                .map_err(|e| SharedSecretLoadError::InvalidHash {
                    path: path.display().to_string(),
                    username: user.username.clone(),
                    detail: e.to_string(),
                })?;
            if users
                .insert(user.username.clone(), user.password_hash)
                .is_some()
            {
                return Err(SharedSecretLoadError::DuplicateUsername {
                    path: path.display().to_string(),
                    username: user.username,
                });
            }
        }
        Ok(users)
    }

    /// Re-read the source file the service was originally
    /// loaded from and atomically swap the in-memory user map
    /// with the freshly-parsed set. Called after
    /// `set_kiosk_password` rewrites the on-disk file so a
    /// step-up verify against the newly-set password succeeds
    /// on the next call without a steward restart.
    ///
    /// Services built via [`Self::from_map`] have no source
    /// path and return `Ok(())` silently — refresh is a no-op.
    /// I/O + parse errors abort the swap; the previous
    /// in-memory state stays intact so an interrupted rewrite
    /// never leaves the service in a half-loaded shape.
    pub fn refresh_from_disk(&self) -> Result<(), SharedSecretLoadError> {
        let Some(path) = self.source_path.as_ref() else {
            return Ok(());
        };
        let text = std::fs::read_to_string(path).map_err(|source| {
            SharedSecretLoadError::Read {
                path: path.display().to_string(),
                source,
            }
        })?;
        let fresh = Self::parse_document(path, &text)?;
        let mut guard = self.users.write().expect("users RwLock poisoned");
        *guard = fresh;
        Ok(())
    }

    /// Load and parse a secret-file from disk. Validates every
    /// `password_hash` at load time so a malformed entry surfaces
    /// before the first verification call.
    pub fn load_from_file(
        path: &std::path::Path,
    ) -> Result<Self, SharedSecretLoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| {
            SharedSecretLoadError::Read {
                path: path.display().to_string(),
                source,
            }
        })?;
        let users = Self::parse_document(path, &text)?;
        Ok(Self {
            users: std::sync::RwLock::new(users),
            source_path: Some(path.to_path_buf()),
        })
    }

    /// Number of users known to this backend. Useful for tests + a
    /// boot-time log line so operators can confirm the loader saw
    /// the expected set.
    pub fn user_count(&self) -> usize {
        self.users.read().expect("users RwLock poisoned").len()
    }
}

impl AuthService for SharedSecretAuthService {
    fn verify(
        &self,
        username: &str,
        secret: &[u8],
    ) -> Result<VerifiedPrincipal, AuthVerificationError> {
        use argon2::password_hash::{PasswordHash, PasswordVerifier};
        // Collapse "unknown user" and "wrong password" to the same
        // outcome and the same code path. The `match` here is
        // deliberately written to flow through `PasswordVerifier`
        // even when the user is unknown by verifying against a
        // sentinel hash; the slight extra work approximates
        // username-independent timing without claiming
        // constant-time correctness (argon2id is intentionally
        // slow, dominating the username-lookup branch).
        let guard = self.users.read().expect("users RwLock poisoned");
        let (stored_hash, user_known) = match guard.get(username) {
            Some(h) => (h.clone(), true),
            None => {
                // Sentinel hash designed to fail verification but
                // exercise the argon2 derivation so the timing of
                // "unknown user" is in the same order as
                // "wrong password".
                (
                    "$argon2id$v=19$m=65536,t=3,p=4\
                     $c29tZXNhbHRzb21lc2FsdHM\
                     $bHFRRkFsbm5LRDZuTk5VbmJYZmJYUVdrNkVDR1pPMU8"
                        .to_string(),
                    false,
                )
            }
        };
        drop(guard);
        let parsed = PasswordHash::new(&stored_hash).map_err(|_| {
            AuthVerificationError::BackendUnavailable {
                reason: "internal: stored hash failed to parse".into(),
            }
        })?;
        match argon2::Argon2::default().verify_password(secret, &parsed) {
            Ok(()) if user_known => Ok(VerifiedPrincipal {
                username: username.to_string(),
                uid: None,
            }),
            _ => Err(AuthVerificationError::InvalidCredentials),
        }
    }

    fn implementation_name(&self) -> &'static str {
        "shared-secret"
    }

    fn refresh_after_secret_write(&self) -> Result<(), String> {
        self.refresh_from_disk().map_err(|e| e.to_string())
    }
}

/// An active privileged session. Issued by [`AuthSessionStore`] on
/// successful verification; the wire layer presents the `token`
/// string on subsequent privileged ClientRequests, and the store
/// validates it before the operation executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedSession {
    /// URL-safe base64 of [`TOKEN_RANDOM_BYTES`] random bytes.
    pub token: String,
    /// Principal the verification call authenticated.
    pub principal: VerifiedPrincipal,
    /// Wall-clock millisecond timestamp at which the session was
    /// issued. Recorded in audit; not used for validity (validity
    /// uses `expires_at_ms` against the current clock).
    pub issued_at_ms: u64,
    /// Wall-clock millisecond timestamp at which the session
    /// expires. Computed at issuance from the requested or default
    /// TTL, capped by the framework ceiling.
    pub expires_at_ms: u64,
    /// OS UID of the peer that performed the verification. Token
    /// validation requires the presenting peer's UID to match this.
    pub bound_peer_uid: u32,
}

/// Validation outcomes. Distinct from
/// [`AuthVerificationError`] — verification produces tokens,
/// validation consumes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionValidationError {
    /// Token does not exist in the store. Either it was never
    /// issued, was revoked, was reaped after expiry, or the
    /// steward restarted since issuance.
    Unknown,
    /// Token exists but its `expires_at_ms` has passed. Reaped on
    /// next access.
    Expired,
    /// Token exists and is not expired but the presenting peer's
    /// UID does not match the UID the token was bound to at
    /// issuance.
    WrongPeer,
}

/// In-memory store of active privileged sessions. Issues tokens on
/// successful verification, validates them on operation execution,
/// revokes on explicit sign-out, reaps expired sessions on access.
#[derive(Debug)]
pub struct AuthSessionStore {
    sessions: Mutex<HashMap<String, PrivilegedSession>>,
    ttl_default: Duration,
    ttl_ceiling: Duration,
}

impl AuthSessionStore {
    /// Construct a new store with the given default TTL, capped at
    /// the framework ceiling. Operators may pass a shorter default
    /// (e.g. via config); the ceiling is locked.
    pub fn new(ttl_default: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            ttl_default: ttl_default.min(STEP_UP_TOKEN_TTL_CEILING),
            ttl_ceiling: STEP_UP_TOKEN_TTL_CEILING,
        }
    }

    /// Default-configured store: 5-minute TTL, 15-minute ceiling.
    pub fn with_defaults() -> Self {
        Self::new(STEP_UP_TOKEN_TTL_DEFAULT)
    }

    /// TTL ceiling enforced by this store. Constant per process.
    pub fn ttl_ceiling(&self) -> Duration {
        self.ttl_ceiling
    }

    /// Default TTL applied when no per-call TTL override is
    /// supplied.
    pub fn ttl_default(&self) -> Duration {
        self.ttl_default
    }

    /// Issue a new session bound to the given principal and peer
    /// UID. `ttl_override` is optional; when present, capped at the
    /// store's ceiling. Returns the issued [`PrivilegedSession`];
    /// the wire layer transmits the `token` to the operator.
    pub fn issue(
        &self,
        principal: VerifiedPrincipal,
        peer_uid: u32,
        ttl_override: Option<Duration>,
    ) -> PrivilegedSession {
        let ttl = ttl_override
            .unwrap_or(self.ttl_default)
            .min(self.ttl_ceiling);
        let now = SystemTime::now();
        let issued_at_ms = system_time_to_ms(now);
        let expires_at_ms = issued_at_ms + ttl.as_millis() as u64;
        let token = generate_token();

        let session = PrivilegedSession {
            token: token.clone(),
            principal,
            issued_at_ms,
            expires_at_ms,
            bound_peer_uid: peer_uid,
        };

        self.sessions
            .lock()
            .expect("AuthSessionStore mutex poisoned")
            .insert(token, session.clone());

        session
    }

    /// Validate a presenting token. Returns the active session on
    /// success; otherwise the appropriate
    /// [`SessionValidationError`]. Expired sessions are reaped
    /// from the store on detection.
    pub fn validate(
        &self,
        token: &str,
        peer_uid: u32,
    ) -> Result<PrivilegedSession, SessionValidationError> {
        let now_ms = system_time_to_ms(SystemTime::now());
        let mut guard = self
            .sessions
            .lock()
            .expect("AuthSessionStore mutex poisoned");
        let session = match guard.get(token) {
            Some(s) => s.clone(),
            None => return Err(SessionValidationError::Unknown),
        };
        if session.expires_at_ms <= now_ms {
            guard.remove(token);
            return Err(SessionValidationError::Expired);
        }
        if session.bound_peer_uid != peer_uid {
            return Err(SessionValidationError::WrongPeer);
        }
        Ok(session)
    }

    /// Revoke a session by token. Returns `true` when a live session
    /// was removed; `false` when the token was unknown or already
    /// expired.
    pub fn revoke(&self, token: &str) -> bool {
        self.sessions
            .lock()
            .expect("AuthSessionStore mutex poisoned")
            .remove(token)
            .is_some()
    }

    /// Reap every expired session. Returns the number of sessions
    /// removed. Called opportunistically on validation; an explicit
    /// invocation is offered for periodic background cleanup.
    pub fn purge_expired(&self) -> usize {
        let now_ms = system_time_to_ms(SystemTime::now());
        let mut guard = self
            .sessions
            .lock()
            .expect("AuthSessionStore mutex poisoned");
        let before = guard.len();
        guard.retain(|_, s| s.expires_at_ms > now_ms);
        before - guard.len()
    }

    /// Snapshot the current session count. Useful for diagnostics
    /// and tests; not exposed on the wire.
    pub fn live_session_count(&self) -> usize {
        self.sessions
            .lock()
            .expect("AuthSessionStore mutex poisoned")
            .len()
    }
}

impl Default for AuthSessionStore {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Maximum step-up verification failures allowed inside one
/// [`STEP_UP_RATE_LIMIT_WINDOW`] before the peer is locked out.
/// After the fifth failure the sixth call returns
/// [`RateLimitOutcome::Locked`] regardless of what secret it
/// presents; the window resets on either explicit success or
/// window expiry.
pub const STEP_UP_RATE_LIMIT_MAX_FAILURES: u32 = 5;

/// Rolling window inside which
/// [`STEP_UP_RATE_LIMIT_MAX_FAILURES`] failed step-up
/// verifications trigger lockout.
pub const STEP_UP_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(15 * 60);

/// TTL applied to a step-up nonce record. Nonce reuse inside the
/// window returns [`NonceOutcome::Reused`]; after the window the
/// nonce is dropped and may be presented again.
pub const STEP_UP_NONCE_TTL: Duration = Duration::from_secs(30);

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitOutcome {
    /// Bucket is under the failure ceiling; verification may
    /// proceed.
    Permitted {
        /// Failures already recorded for this identity inside the
        /// current window (excluding the call that would follow).
        failures_in_window: u32,
    },
    /// Bucket is at or above the ceiling; verification is refused
    /// without contacting the AuthService. `retry_after_ms`
    /// carries the milliseconds until the earliest recorded
    /// failure ages out of the window.
    Locked {
        /// Milliseconds until the earliest failure ages out and
        /// the peer regains an attempt slot.
        retry_after_ms: u64,
    },
}

/// Outcome of a nonce-recording call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NonceOutcome {
    /// Nonce was not present in the store; recorded now with the
    /// configured TTL. Verification may proceed.
    Fresh,
    /// Nonce was already in the store and its TTL has not elapsed.
    /// Verification refused as replay.
    Reused,
}

/// Failed step-up attempts recorded per identity so the framework
/// can refuse further calls when the rate ceiling is crossed.
/// Identity is a caller-chosen string: on the Unix socket the
/// convention is `uid:<peer-uid>`; on the WSS surface the
/// convention is `bearer:<bearer-token-id>`. The store does not
/// interpret the identity — it is opaque.
#[derive(Debug, Default)]
pub struct StepUpRateLimiter {
    buckets: Mutex<HashMap<String, Vec<u64>>>,
    max_failures: u32,
    window: Duration,
}

impl StepUpRateLimiter {
    /// Construct a limiter with the framework defaults
    /// ([`STEP_UP_RATE_LIMIT_MAX_FAILURES`] +
    /// [`STEP_UP_RATE_LIMIT_WINDOW`]).
    pub fn with_defaults() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            max_failures: STEP_UP_RATE_LIMIT_MAX_FAILURES,
            window: STEP_UP_RATE_LIMIT_WINDOW,
        }
    }

    /// Construct a limiter with an explicit failure ceiling +
    /// window. Intended for tests; production callers use
    /// [`Self::with_defaults`].
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            max_failures,
            window,
        }
    }

    /// Consult the bucket for an identity. Returns whether the
    /// verification is permitted (and what the current in-window
    /// failure count is) or locked (and how long until the peer
    /// gets a slot back). Does NOT record a new failure; callers
    /// record after the verify result is known.
    pub fn check(&self, identity: &str, now_ms: u64) -> RateLimitOutcome {
        let mut guard = self
            .buckets
            .lock()
            .expect("StepUpRateLimiter mutex poisoned");
        let window_ms = self.window.as_millis() as u64;
        let cutoff_ms = now_ms.saturating_sub(window_ms);
        if let Some(bucket) = guard.get_mut(identity) {
            bucket.retain(|&t| t > cutoff_ms);
            if bucket.len() as u32 >= self.max_failures {
                let earliest = *bucket
                    .iter()
                    .min()
                    .expect("non-empty bucket has a minimum");
                let retry_after_ms =
                    earliest.saturating_add(window_ms).saturating_sub(now_ms);
                return RateLimitOutcome::Locked { retry_after_ms };
            }
            RateLimitOutcome::Permitted {
                failures_in_window: bucket.len() as u32,
            }
        } else {
            RateLimitOutcome::Permitted {
                failures_in_window: 0,
            }
        }
    }

    /// Record a failure against `identity` at `now_ms`.
    /// Subsequent [`Self::check`] calls consult the updated
    /// bucket.
    pub fn record_failure(&self, identity: &str, now_ms: u64) {
        let mut guard = self
            .buckets
            .lock()
            .expect("StepUpRateLimiter mutex poisoned");
        guard.entry(identity.to_string()).or_default().push(now_ms);
    }

    /// Clear the bucket for `identity`. Called on a successful
    /// step-up so the correct-password path does not carry
    /// past-failure penalties.
    pub fn record_success(&self, identity: &str) {
        self.buckets
            .lock()
            .expect("StepUpRateLimiter mutex poisoned")
            .remove(identity);
    }

    /// Reap identities whose most-recent failure is older than
    /// the window (housekeeping; not required for correctness
    /// because [`Self::check`] prunes lazily).
    pub fn purge_expired(&self, now_ms: u64) -> usize {
        let mut guard = self
            .buckets
            .lock()
            .expect("StepUpRateLimiter mutex poisoned");
        let window_ms = self.window.as_millis() as u64;
        let cutoff_ms = now_ms.saturating_sub(window_ms);
        let before = guard.len();
        guard.retain(|_, bucket| {
            bucket.retain(|&t| t > cutoff_ms);
            !bucket.is_empty()
        });
        before - guard.len()
    }
}

/// Replay defence for step-up verify calls. Every
/// `step_up_auth_verify` carries a caller-generated nonce; the
/// framework records the nonce with [`STEP_UP_NONCE_TTL`] and
/// refuses reuse inside that window. Prevents an attacker who
/// captures one on-the-wire secret payload from replaying the
/// same payload against the framework to obtain a fresh session
/// token in the operator's name.
#[derive(Debug, Default)]
pub struct NonceReplayStore {
    seen: Mutex<HashMap<String, u64>>,
    ttl: Duration,
}

impl NonceReplayStore {
    /// Construct a store with the framework default nonce TTL.
    pub fn with_defaults() -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            ttl: STEP_UP_NONCE_TTL,
        }
    }

    /// Construct a store with an explicit TTL. For tests.
    pub fn new(ttl: Duration) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Record a nonce presentation. Returns
    /// [`NonceOutcome::Fresh`] when the nonce was not present;
    /// [`NonceOutcome::Reused`] when it was present and within
    /// TTL. Reaps expired nonces on every call.
    pub fn record(&self, nonce: &str, now_ms: u64) -> NonceOutcome {
        let mut guard =
            self.seen.lock().expect("NonceReplayStore mutex poisoned");
        let ttl_ms = self.ttl.as_millis() as u64;
        // Reap expired entries every call — bounded work because
        // the map size is capped by ttl × per-second call rate.
        guard.retain(|_, &mut expires_at| expires_at > now_ms);
        if let Some(&expires_at) = guard.get(nonce) {
            if expires_at > now_ms {
                return NonceOutcome::Reused;
            }
        }
        guard.insert(nonce.to_string(), now_ms.saturating_add(ttl_ms));
        NonceOutcome::Fresh
    }

    /// Current number of live (unexpired) nonce records.
    pub fn live_count(&self, now_ms: u64) -> usize {
        let guard = self.seen.lock().expect("NonceReplayStore mutex poisoned");
        guard.values().filter(|&&t| t > now_ms).count()
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_RANDOM_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    B64.encode(bytes)
}

fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(name: &str) -> VerifiedPrincipal {
        VerifiedPrincipal {
            username: name.into(),
            uid: Some(1000),
        }
    }

    fn hash(secret: &str) -> String {
        use argon2::password_hash::{
            rand_core::OsRng, PasswordHasher, SaltString,
        };
        let salt = SaltString::generate(&mut OsRng);
        argon2::Argon2::default()
            .hash_password(secret.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    #[test]
    fn shared_secret_auth_service_accepts_correct_password() {
        let mut users = HashMap::new();
        users.insert("operator".to_string(), hash("correct horse battery"));
        let svc = SharedSecretAuthService::from_map(users);
        let result = svc.verify("operator", b"correct horse battery");
        assert!(result.is_ok(), "verify failed: {result:?}");
        let p = result.unwrap();
        assert_eq!(p.username, "operator");
        assert_eq!(p.uid, None);
        assert_eq!(svc.implementation_name(), "shared-secret");
    }

    #[test]
    fn shared_secret_auth_service_rejects_wrong_password() {
        let mut users = HashMap::new();
        users.insert("operator".to_string(), hash("correct"));
        let svc = SharedSecretAuthService::from_map(users);
        let err = svc.verify("operator", b"wrong").unwrap_err();
        assert!(matches!(err, AuthVerificationError::InvalidCredentials));
    }

    #[test]
    fn shared_secret_auth_service_rejects_unknown_user() {
        let mut users = HashMap::new();
        users.insert("operator".to_string(), hash("secret"));
        let svc = SharedSecretAuthService::from_map(users);
        let err = svc.verify("nobody", b"anything").unwrap_err();
        assert!(matches!(err, AuthVerificationError::InvalidCredentials));
    }

    #[test]
    fn shared_secret_auth_service_load_from_file_parses_valid_toml() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("auth-secrets.toml");
        let h = hash("op-password");
        std::fs::write(
            &path,
            format!(
                "schema_version = 1\n\n[[users]]\n\
                 username = \"operator\"\npassword_hash = \"{h}\"\n"
            ),
        )
        .unwrap();
        let svc = SharedSecretAuthService::load_from_file(&path)
            .expect("load_from_file");
        assert_eq!(svc.user_count(), 1);
        assert!(svc.verify("operator", b"op-password").is_ok());
    }

    #[test]
    fn shared_secret_refresh_from_disk_picks_up_new_hash_without_restart() {
        // Regression for the set_kiosk_password walk defect: the
        // handler rewrites the on-disk hash; the in-memory
        // AuthService must reload so a step-up verify against
        // the newly-set password succeeds on the next call
        // without a steward restart.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("secrets.toml");
        let old_hash = hash("old-password");
        std::fs::write(
            &path,
            format!(
                "schema_version = 1\n\n[[users]]\n\
                 username = \"operator\"\npassword_hash = \"{old_hash}\"\n"
            ),
        )
        .unwrap();
        let svc = SharedSecretAuthService::load_from_file(&path)
            .expect("initial load");
        // Baseline: old password verifies, new password does not.
        assert!(svc.verify("operator", b"old-password").is_ok());
        assert!(svc.verify("operator", b"new-password").is_err());
        // Rewrite the file with a new hash — same shape the
        // set_kiosk_password handler produces.
        let new_hash = hash("new-password");
        std::fs::write(
            &path,
            format!(
                "schema_version = 1\n\n[[users]]\n\
                 username = \"operator\"\npassword_hash = \"{new_hash}\"\n"
            ),
        )
        .unwrap();
        // Without refresh, in-memory state stays at the old hash.
        assert!(svc.verify("operator", b"new-password").is_err());
        assert!(svc.verify("operator", b"old-password").is_ok());
        // Refresh atomically swaps in the new set.
        svc.refresh_from_disk().expect("refresh should succeed");
        // Post-refresh: new password verifies, old does not.
        assert!(svc.verify("operator", b"new-password").is_ok());
        assert!(svc.verify("operator", b"old-password").is_err());
        assert_eq!(svc.user_count(), 1);
    }

    #[test]
    fn shared_secret_refresh_from_map_service_is_noop() {
        // A service constructed via from_map has no source path;
        // the trait-level refresh_after_secret_write default and
        // the concrete refresh_from_disk both return Ok(()) so
        // set_kiosk_password does not fail on tests wiring a
        // non-file-backed backend.
        let mut users = HashMap::new();
        users.insert("operator".to_string(), hash("password"));
        let svc = SharedSecretAuthService::from_map(users);
        assert!(svc.refresh_from_disk().is_ok());
        assert!(svc.refresh_after_secret_write().is_ok());
        assert!(svc.verify("operator", b"password").is_ok());
    }

    #[test]
    fn shared_secret_refresh_from_disk_atomic_on_parse_failure() {
        // A mid-flight parse failure during refresh must not
        // corrupt the in-memory state — the previous set stays
        // intact so verification continues to work.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("secrets.toml");
        let h = hash("stable-password");
        std::fs::write(
            &path,
            format!(
                "schema_version = 1\n\n[[users]]\n\
                 username = \"operator\"\npassword_hash = \"{h}\"\n"
            ),
        )
        .unwrap();
        let svc = SharedSecretAuthService::load_from_file(&path)
            .expect("initial load");
        assert!(svc.verify("operator", b"stable-password").is_ok());
        // Rewrite with malformed TOML.
        std::fs::write(&path, "not = valid = toml").unwrap();
        // Refresh returns an error and the in-memory state is
        // unchanged.
        assert!(svc.refresh_from_disk().is_err());
        assert!(svc.verify("operator", b"stable-password").is_ok());
    }

    #[test]
    fn shared_secret_auth_service_load_rejects_unsupported_schema_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("v2.toml");
        std::fs::write(&path, "schema_version = 2\n").unwrap();
        match SharedSecretAuthService::load_from_file(&path).unwrap_err() {
            SharedSecretLoadError::UnsupportedSchemaVersion {
                found, ..
            } => {
                assert_eq!(found, 2);
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }

    #[test]
    fn shared_secret_auth_service_load_rejects_invalid_hash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("bad-hash.toml");
        std::fs::write(
            &path,
            "schema_version = 1\n\n[[users]]\n\
             username = \"alice\"\npassword_hash = \"not-a-phc-hash\"\n",
        )
        .unwrap();
        match SharedSecretAuthService::load_from_file(&path).unwrap_err() {
            SharedSecretLoadError::InvalidHash { username, .. } => {
                assert_eq!(username, "alice");
            }
            other => panic!("expected InvalidHash, got {other:?}"),
        }
    }

    #[test]
    fn shared_secret_auth_service_load_rejects_duplicate_usernames() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("dup.toml");
        let h = hash("x");
        std::fs::write(
            &path,
            format!(
                "schema_version = 1\n\
                 [[users]]\nusername = \"a\"\npassword_hash = \"{h}\"\n\
                 [[users]]\nusername = \"a\"\npassword_hash = \"{h}\"\n"
            ),
        )
        .unwrap();
        match SharedSecretAuthService::load_from_file(&path).unwrap_err() {
            SharedSecretLoadError::DuplicateUsername { username, .. } => {
                assert_eq!(username, "a");
            }
            other => panic!("expected DuplicateUsername, got {other:?}"),
        }
    }

    #[test]
    fn no_auth_service_denies_with_backend_unavailable() {
        let svc = NoAuthService;
        let err = svc.verify("alice", b"secret").unwrap_err();
        match err {
            AuthVerificationError::BackendUnavailable { reason } => {
                assert!(reason.contains("no AuthService configured"));
            }
            other => panic!("expected BackendUnavailable, got {other:?}"),
        }
        assert_eq!(svc.implementation_name(), "none");
    }

    #[test]
    fn store_issue_yields_unique_url_safe_token() {
        let store = AuthSessionStore::with_defaults();
        let s1 = store.issue(principal("alice"), 1000, None);
        let s2 = store.issue(principal("alice"), 1000, None);
        assert_ne!(s1.token, s2.token);
        // URL-safe base64, no padding: only [A-Za-z0-9_-]
        for ch in s1.token.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "token contained non-url-safe char: {ch:?}"
            );
        }
    }

    #[test]
    fn store_caps_ttl_at_ceiling() {
        let store = AuthSessionStore::with_defaults();
        let session = store.issue(
            principal("alice"),
            1000,
            Some(Duration::from_secs(60 * 60 * 24)),
        );
        let actual_ttl_ms = session.expires_at_ms - session.issued_at_ms;
        let ceiling_ms = STEP_UP_TOKEN_TTL_CEILING.as_millis() as u64;
        assert!(
            actual_ttl_ms <= ceiling_ms,
            "TTL {actual_ttl_ms}ms exceeds ceiling {ceiling_ms}ms"
        );
        assert!(
            actual_ttl_ms >= ceiling_ms - 1000,
            "TTL {actual_ttl_ms}ms unexpectedly below ceiling \
             {ceiling_ms}ms"
        );
    }

    #[test]
    fn store_default_ctor_caps_default_at_ceiling() {
        let store = AuthSessionStore::new(Duration::from_secs(60 * 60 * 24));
        assert_eq!(store.ttl_default(), STEP_UP_TOKEN_TTL_CEILING);
    }

    #[test]
    fn validate_returns_session_for_valid_token() {
        let store = AuthSessionStore::with_defaults();
        let issued = store.issue(principal("alice"), 1000, None);
        let validated = store.validate(&issued.token, 1000).unwrap();
        assert_eq!(validated.token, issued.token);
        assert_eq!(validated.principal, issued.principal);
    }

    #[test]
    fn validate_unknown_token_errors_unknown() {
        let store = AuthSessionStore::with_defaults();
        let err = store.validate("not-a-real-token", 1000).unwrap_err();
        assert_eq!(err, SessionValidationError::Unknown);
    }

    #[test]
    fn validate_wrong_peer_errors_wrong_peer() {
        let store = AuthSessionStore::with_defaults();
        let issued = store.issue(principal("alice"), 1000, None);
        let err = store.validate(&issued.token, 2000).unwrap_err();
        assert_eq!(err, SessionValidationError::WrongPeer);
    }

    #[test]
    fn validate_expired_token_errors_expired_and_reaps() {
        let store = AuthSessionStore::new(Duration::from_millis(1));
        let issued = store.issue(principal("alice"), 1000, None);
        std::thread::sleep(Duration::from_millis(10));
        let err = store.validate(&issued.token, 1000).unwrap_err();
        assert_eq!(err, SessionValidationError::Expired);
        // Subsequent validation should report Unknown — reaped.
        let err2 = store.validate(&issued.token, 1000).unwrap_err();
        assert_eq!(err2, SessionValidationError::Unknown);
    }

    #[test]
    fn revoke_removes_active_session() {
        let store = AuthSessionStore::with_defaults();
        let issued = store.issue(principal("alice"), 1000, None);
        assert_eq!(store.live_session_count(), 1);
        assert!(store.revoke(&issued.token));
        assert_eq!(store.live_session_count(), 0);
        let err = store.validate(&issued.token, 1000).unwrap_err();
        assert_eq!(err, SessionValidationError::Unknown);
    }

    #[test]
    fn revoke_unknown_returns_false() {
        let store = AuthSessionStore::with_defaults();
        assert!(!store.revoke("does-not-exist"));
    }

    #[test]
    fn purge_expired_removes_only_expired() {
        let store = AuthSessionStore::with_defaults();
        let _alive = store.issue(principal("alice"), 1000, None);

        let short = AuthSessionStore::new(Duration::from_millis(1));
        let _doomed = short.issue(principal("bob"), 1001, None);
        std::thread::sleep(Duration::from_millis(10));

        assert_eq!(store.purge_expired(), 0);
        assert_eq!(store.live_session_count(), 1);
        assert_eq!(short.purge_expired(), 1);
        assert_eq!(short.live_session_count(), 0);
    }

    #[test]
    fn rate_limiter_permits_first_call_and_reports_zero_failures() {
        let lim = StepUpRateLimiter::with_defaults();
        let outcome = lim.check("uid:1000", 1_000_000);
        assert_eq!(
            outcome,
            RateLimitOutcome::Permitted {
                failures_in_window: 0
            }
        );
    }

    #[test]
    fn rate_limiter_locks_at_max_failures_and_reports_retry_after() {
        let lim = StepUpRateLimiter::new(3, Duration::from_secs(60));
        // Three failures inside the window.
        lim.record_failure("uid:1000", 1_000_000);
        lim.record_failure("uid:1000", 1_000_100);
        lim.record_failure("uid:1000", 1_000_200);
        // Fourth check refuses; retry-after is measured from the
        // earliest recorded failure.
        let outcome = lim.check("uid:1000", 1_000_300);
        match outcome {
            RateLimitOutcome::Locked { retry_after_ms } => {
                // Window is 60s = 60_000ms; earliest was at
                // 1_000_000; retry_after = (1_000_000 + 60_000) -
                // 1_000_300 = 59_700.
                assert_eq!(retry_after_ms, 59_700);
            }
            other => panic!("expected Locked, got {other:?}"),
        }
    }

    #[test]
    fn rate_limiter_reset_on_success_clears_bucket() {
        let lim = StepUpRateLimiter::new(3, Duration::from_secs(60));
        lim.record_failure("uid:1000", 1_000_000);
        lim.record_failure("uid:1000", 1_000_100);
        lim.record_success("uid:1000");
        let outcome = lim.check("uid:1000", 1_000_200);
        assert_eq!(
            outcome,
            RateLimitOutcome::Permitted {
                failures_in_window: 0
            }
        );
    }

    #[test]
    fn rate_limiter_prunes_expired_failures_on_check() {
        let lim = StepUpRateLimiter::new(3, Duration::from_secs(60));
        lim.record_failure("uid:1000", 1_000_000);
        lim.record_failure("uid:1000", 1_000_100);
        lim.record_failure("uid:1000", 1_000_200);
        // Now moved past the window relative to the LAST failure
        // — all three failures age out at once.
        let outcome = lim.check("uid:1000", 1_000_200 + 60_001);
        assert_eq!(
            outcome,
            RateLimitOutcome::Permitted {
                failures_in_window: 0
            }
        );
    }

    #[test]
    fn rate_limiter_partial_aging_leaves_recent_failures_only() {
        let lim = StepUpRateLimiter::new(3, Duration::from_secs(60));
        lim.record_failure("uid:1000", 1_000_000);
        lim.record_failure("uid:1000", 1_030_000); // 30 s later
        lim.record_failure("uid:1000", 1_050_000); // 50 s in
                                                   // Move to 1_061_000 — window is [1_001_000, 1_061_000],
                                                   // so only the first failure ages out. Two remain.
        let outcome = lim.check("uid:1000", 1_061_000);
        assert_eq!(
            outcome,
            RateLimitOutcome::Permitted {
                failures_in_window: 2
            }
        );
    }

    #[test]
    fn rate_limiter_isolates_identities() {
        let lim = StepUpRateLimiter::new(3, Duration::from_secs(60));
        for _ in 0..3 {
            lim.record_failure("uid:1000", 1_000_000);
        }
        // Locked for uid:1000 …
        assert!(matches!(
            lim.check("uid:1000", 1_000_100),
            RateLimitOutcome::Locked { .. }
        ));
        // … but a different identity is unaffected.
        assert_eq!(
            lim.check("uid:1001", 1_000_100),
            RateLimitOutcome::Permitted {
                failures_in_window: 0
            }
        );
    }

    #[test]
    fn nonce_store_permits_fresh_nonce_once() {
        let store = NonceReplayStore::with_defaults();
        assert_eq!(store.record("abc", 1_000_000), NonceOutcome::Fresh);
    }

    #[test]
    fn nonce_store_refuses_reuse_inside_window() {
        let store = NonceReplayStore::new(Duration::from_secs(30));
        assert_eq!(store.record("abc", 1_000_000), NonceOutcome::Fresh);
        assert_eq!(store.record("abc", 1_000_100), NonceOutcome::Reused);
    }

    #[test]
    fn nonce_store_permits_reuse_after_ttl_elapses() {
        let store = NonceReplayStore::new(Duration::from_secs(30));
        assert_eq!(store.record("abc", 1_000_000), NonceOutcome::Fresh);
        // 30s TTL = 30_000ms; move past it.
        assert_eq!(
            store.record("abc", 1_000_000 + 30_001),
            NonceOutcome::Fresh
        );
    }

    #[test]
    fn nonce_store_live_count_excludes_expired() {
        let store = NonceReplayStore::new(Duration::from_secs(30));
        store.record("a", 1_000_000);
        store.record("b", 1_000_000);
        assert_eq!(store.live_count(1_000_100), 2);
        assert_eq!(store.live_count(1_000_000 + 30_001), 0);
    }
}
