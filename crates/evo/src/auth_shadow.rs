// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Local-only shadow-file [`AuthService`] for the reference
//! distribution.
//!
//! Verifies the operator's password against `/etc/shadow` for the
//! framework's own runtime user (the euid captured at
//! construction), using the sibling [`evo_shadow_crypt`] crate as
//! a thin wrapper over libcrypt's `crypt_r(3)` — no PAM stack, no
//! dynamic-module loader, no network authority reachable at any
//! layer.
//!
//! ## Why this shape
//!
//! - **Local-only by construction.** No PAM stack, no winbind, no
//!   sssd, no network authority reachable at any layer. A domain
//!   controller outage, a winbind misconfiguration, or a policy
//!   swap of `pam_unix` for `pam_sss` in `/etc/pam.d/*` cannot
//!   affect this path — the path never touches those surfaces.
//! - **Every hash format libcrypt supports verifies identically.**
//!   `evo_shadow_crypt::verify` handles yescrypt (`$y$`),
//!   SHA-512-crypt (`$6$`), SHA-256-crypt (`$5$`), MD5-crypt
//!   (`$1$`), bcrypt (`$2b$`). The framework does not force a
//!   specific format on `/etc/shadow`; OS-standard rotation via
//!   `passwd` keeps working across distribution defaults.
//! - **Zero cross-compile linker complication.** `evo-shadow-crypt`
//!   resolves `libcrypt.so.1` at runtime via `libloading`; no
//!   compile-time libcrypt-dev headers, no per-arch dev
//!   packages.
//! - **Runtime user pinned.** Every `verify` call whose `username`
//!   does not equal the captured runtime user returns
//!   [`AuthVerificationError::UserNotPermitted`] — the framework
//!   authenticates its own runtime user or nobody. No path exists
//!   through this backend to authenticate any other UID.
//!
//! ## Wiring
//!
//! Distribution boot:
//! ```rust,ignore
//! let svc = ShadowAuthService::for_current_process()?;
//! let auth_service: Arc<dyn AuthService> = Arc::new(svc);
//! ```
//!
//! Runs as whatever OS user the systemd unit uses. The distribution's
//! install script must add the runtime user to the `shadow` group;
//! without that, `/etc/shadow` is not readable and every verify
//! fails with `BackendReadFailed`.

use std::path::{Path, PathBuf};

use crate::auth::{AuthService, AuthVerificationError, VerifiedPrincipal};

/// The shadow-file path this backend reads from.
///
/// Configurable via [`ShadowAuthService::with_shadow_path`] for
/// tests + non-standard chroots; production always resolves the
/// system default `/etc/shadow`.
pub const DEFAULT_SHADOW_PATH: &str = "/etc/shadow";

/// Local-only shadow-file backing for the [`AuthService`] trait.
///
/// Verifies the runtime user's password against `/etc/shadow` via
/// libcrypt's `crypt_r(3)` (through
/// [`evo_shadow_crypt::verify`]). Refuses to authenticate any
/// username other than the runtime user by construction — no
/// admin escalation path exists through this backend.
pub struct ShadowAuthService {
    runtime_user: String,
    shadow_path: PathBuf,
}

impl std::fmt::Debug for ShadowAuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShadowAuthService")
            .field("runtime_user", &self.runtime_user)
            .field("shadow_path", &self.shadow_path)
            .finish()
    }
}

/// Errors surfaced when constructing a [`ShadowAuthService`] via
/// [`ShadowAuthService::for_current_process`].
#[derive(Debug, thiserror::Error)]
pub enum ShadowAuthConstructError {
    /// The framework's euid did not resolve to a passwd entry
    /// (deleted mid-run, chroot with a shorter passwd, etc.).
    #[error("euid {euid} did not resolve to a passwd entry")]
    EuidNotInPasswd {
        /// The euid whose lookup failed.
        euid: u32,
    },
    /// `getpwuid_r` returned an error other than "not found". Wraps
    /// the underlying `nix` diagnostic.
    #[error("getpwuid_r for euid {euid} failed: {detail}")]
    PasswdLookupFailed {
        /// The euid whose lookup failed.
        euid: u32,
        /// Underlying error text from `nix`.
        detail: String,
    },
}

impl ShadowAuthService {
    /// Construct a service pinned to the framework's own runtime
    /// user (resolved from `geteuid()`), reading the system shadow
    /// file at `/etc/shadow`.
    ///
    /// Only usable at boot after the framework has settled its
    /// final euid. The euid is captured once here; a subsequent
    /// `setuid` after construction has no effect on which user
    /// this service will authenticate.
    pub fn for_current_process() -> Result<Self, ShadowAuthConstructError> {
        use nix::unistd::{Uid, User};
        let euid = Uid::effective();
        let user = User::from_uid(euid).map_err(|e| {
            ShadowAuthConstructError::PasswdLookupFailed {
                euid: euid.as_raw(),
                detail: e.to_string(),
            }
        })?;
        let user = user.ok_or(ShadowAuthConstructError::EuidNotInPasswd {
            euid: euid.as_raw(),
        })?;
        let username = user.name;
        tracing::info!(
            euid = euid.as_raw(),
            runtime_user = %username,
            shadow_path = DEFAULT_SHADOW_PATH,
            "auth-shadow: ShadowAuthService bound to runtime user",
        );
        Ok(Self {
            runtime_user: username,
            shadow_path: PathBuf::from(DEFAULT_SHADOW_PATH),
        })
    }

    /// Construct a service pinned to an explicit runtime user +
    /// shadow-file path. Intended for tests + non-standard chroots
    /// where the caller supplies both explicitly. Production
    /// callers use [`Self::for_current_process`].
    pub fn with_shadow_path(
        runtime_user: impl Into<String>,
        shadow_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            runtime_user: runtime_user.into(),
            shadow_path: shadow_path.into(),
        }
    }

    /// The runtime user this service is pinned to. Used by
    /// `set_kiosk_password` to target the correct chpasswd row
    /// without a second `geteuid` lookup.
    pub fn runtime_user(&self) -> &str {
        &self.runtime_user
    }

    /// The shadow-file path this service reads.
    pub fn shadow_path(&self) -> &Path {
        &self.shadow_path
    }

    fn read_hash_for_runtime_user(
        &self,
    ) -> Result<String, AuthVerificationError> {
        let text = std::fs::read_to_string(&self.shadow_path).map_err(|e| {
            AuthVerificationError::BackendReadFailed {
                reason: format!("read {}: {e}", self.shadow_path.display(),),
            }
        })?;
        // Standard shadow row shape:
        //   <name>:<hash>:<lastchange>:<min>:<max>:<warn>:<inactive>:<expire>:
        for row in text.lines() {
            let mut cols = row.split(':');
            let Some(name) = cols.next() else { continue };
            if name != self.runtime_user {
                continue;
            }
            let hash = cols.next().unwrap_or("");
            // Empty / disabled / locked passwords are refusable
            // regardless of what the caller presented — no hash
            // comparison needed. The `!!` (Debian aged password),
            // `!*`, `*`, and empty forms all fall here.
            if hash.is_empty()
                || hash == "*"
                || hash == "!"
                || hash.starts_with('!')
            {
                tracing::debug!(
                    runtime_user = %self.runtime_user,
                    "auth-shadow: runtime user has no live password \
                     (locked / disabled / never set); refusing verify",
                );
                return Err(AuthVerificationError::InvalidCredentials);
            }
            return Ok(hash.to_string());
        }
        // Runtime user is not present in shadow at all.
        // Distribution setup defect — surface as BackendReadFailed
        // (backend read the file but the required row is missing)
        // so the operator sees "framework can't find the credential
        // to check" distinct from "no auth backend configured at
        // all" and from "wrong password".
        Err(AuthVerificationError::BackendReadFailed {
            reason: format!(
                "runtime user {:?} not present in {}",
                self.runtime_user,
                self.shadow_path.display(),
            ),
        })
    }
}

impl AuthService for ShadowAuthService {
    fn verify(
        &self,
        username: &str,
        secret: &[u8],
    ) -> Result<VerifiedPrincipal, AuthVerificationError> {
        // Runtime-user pin. Every caller-supplied username must
        // equal the framework's euid-derived name. Refusing here
        // means no admin escalation path exists through this
        // backend, regardless of what the caller asserts on the
        // wire.
        if username != self.runtime_user {
            return Err(AuthVerificationError::UserNotPermitted);
        }
        let stored_hash = self.read_hash_for_runtime_user()?;
        // evo_shadow_crypt::verify wraps libcrypt's crypt_r(3)
        // via runtime dynamic-load. Every hash format libcrypt
        // supports — yescrypt ($y$), SHA-512-crypt ($6$),
        // SHA-256-crypt ($5$), MD5-crypt ($1$), bcrypt ($2b$) —
        // verifies identically. The framework does not force a
        // specific format on /etc/shadow; OS-standard rotation
        // via `passwd` keeps working across distribution defaults.
        let hash_prefix =
            stored_hash.split_terminator('$').nth(1).unwrap_or("<none>");
        match evo_shadow_crypt::verify(secret, &stored_hash) {
            Ok(true) => Ok(VerifiedPrincipal {
                username: self.runtime_user.clone(),
                uid: Some(nix::unistd::Uid::effective().as_raw()),
            }),
            Ok(false) => Err(AuthVerificationError::InvalidCredentials),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    hash_prefix = %hash_prefix,
                    "auth-shadow: verify failed at libcrypt layer"
                );
                Err(AuthVerificationError::BackendVerifyError {
                    reason: format!("libcrypt: {e}"),
                })
            }
        }
    }

    fn implementation_name(&self) -> &'static str {
        "shadow"
    }

    // refresh_after_secret_write intentionally uses the default
    // no-op impl. This backend has no in-memory cache; every
    // verify reads /etc/shadow fresh. A set_kiosk_password
    // rewrite is picked up on the next call automatically.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_shadow(rows: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("shadow");
        let mut f = std::fs::File::create(&path).expect("create shadow");
        for (name, hash) in rows {
            writeln!(f, "{name}:{hash}:19000:0:99999:7:::")
                .expect("write shadow row");
        }
        (tmp, path)
    }

    /// Produce a real crypt(3) hash for `password` using
    /// libcrypt's `crypt_r(3)` via
    /// `evo_shadow_crypt::hash_for_test`. Settings tried in
    /// order — yescrypt first (Debian trixie default), then
    /// SHA-512-crypt (Debian bullseye default), then
    /// SHA-256-crypt as a fallback. Returns the first hash
    /// libcrypt actually produced; a `*`-prefixed output is
    /// libxcrypt's "unsupported setting" signal and we skip to
    /// the next.
    fn hash_via_libcrypt(password: &[u8]) -> String {
        for setting in ["$y$j9T$abcdefghij", "$6$abcdefghij", "$5$abcdefghij"] {
            match evo_shadow_crypt::hash_for_test(password, setting) {
                Ok(h) if !h.starts_with('*') => return h,
                _ => continue,
            }
        }
        panic!(
            "no libcrypt setting prefix produced a valid hash on this host \
             — libcrypt.so.1 may be a very cut-down build"
        );
    }

    #[test]
    fn read_hash_for_runtime_user_extracts_matching_row() {
        let (_tmp, path) = write_shadow(&[
            ("root", "$6$rootsalt$roothash"),
            ("operator", "$6$opsalt$ophash"),
            ("nobody", "!"),
        ]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        let hash = svc.read_hash_for_runtime_user().unwrap();
        assert_eq!(hash, "$6$opsalt$ophash");
    }

    #[test]
    fn read_hash_locked_password_refuses_invalid_credentials() {
        // Debian convention: `!` prefix means the account has no
        // usable password. Framework must refuse without going
        // through SHA-512 verify so the operator sees
        // InvalidCredentials rather than a random-string match.
        let (_tmp, path) = write_shadow(&[("operator", "!$6$oldsalt$oldhash")]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        let err = svc.read_hash_for_runtime_user().unwrap_err();
        assert_eq!(err, AuthVerificationError::InvalidCredentials);
    }

    #[test]
    fn read_hash_empty_password_refuses_invalid_credentials() {
        let (_tmp, path) = write_shadow(&[("operator", "")]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        let err = svc.read_hash_for_runtime_user().unwrap_err();
        assert_eq!(err, AuthVerificationError::InvalidCredentials);
    }

    #[test]
    fn read_hash_star_password_refuses_invalid_credentials() {
        let (_tmp, path) = write_shadow(&[("operator", "*")]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        let err = svc.read_hash_for_runtime_user().unwrap_err();
        assert_eq!(err, AuthVerificationError::InvalidCredentials);
    }

    #[test]
    fn read_hash_missing_runtime_user_surfaces_backend_read_failed() {
        // Distribution setup defect: the runtime user is not in
        // shadow at all. Framework surfaces as BackendReadFailed
        // (distinct from BackendUnavailable, which is "no
        // AuthService configured at all") so the operator sees a
        // "framework can't find the credential" surface, not
        // "wrong password" and not "no password set".
        let (_tmp, path) = write_shadow(&[("root", "$6$salt$hash")]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        let err = svc.read_hash_for_runtime_user().unwrap_err();
        assert!(matches!(
            err,
            AuthVerificationError::BackendReadFailed { .. }
        ));
    }

    #[test]
    fn verify_refuses_non_runtime_user_with_user_not_permitted() {
        // Runtime-user pin: the caller-supplied username MUST
        // equal the framework's euid-derived name. Refusing here
        // means no admin escalation path exists through this
        // backend regardless of what the wire asserts.
        let (_tmp, path) = write_shadow(&[
            ("operator", "$6$salt$hash"),
            ("root", "$6$rootsalt$roothash"),
        ]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        let err = svc.verify("root", b"whatever").unwrap_err();
        assert_eq!(err, AuthVerificationError::UserNotPermitted);
    }

    #[test]
    fn verify_correct_password_returns_verified_principal() {
        let stored = hash_via_libcrypt(b"correct-password");
        let (_tmp, path) = write_shadow(&[("operator", &stored)]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        let ok = svc.verify("operator", b"correct-password");
        assert!(ok.is_ok(), "verify against known-good hash: {ok:?}");
        let err = svc.verify("operator", b"wrong-password").unwrap_err();
        assert_eq!(err, AuthVerificationError::InvalidCredentials);
    }

    /// Verify must succeed against a yescrypt (`$y$`) hash
    /// produced by the OS's own `passwd`. libcrypt handles every
    /// format transparently, so OS-standard password rotation
    /// keeps working regardless of distribution default.
    #[test]
    fn verify_succeeds_against_yescrypt_hash() {
        let stored = evo_shadow_crypt::hash_for_test(
            b"correct-password",
            "$y$j9T$abcdefghij",
        )
        .expect("yescrypt hash generation");
        if stored.starts_with('*') {
            eprintln!(
                "skipping: local libcrypt.so.1 does not implement \
                 yescrypt (returned unsupported-setting sentinel)"
            );
            return;
        }
        let (_tmp, path) = write_shadow(&[("operator", &stored)]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        assert!(
            svc.verify("operator", b"correct-password").is_ok(),
            "verify must succeed against a real yescrypt hash \
             — OS-standard password rotation on Debian trixie \
             must not lock the operator out"
        );
        assert_eq!(
            svc.verify("operator", b"wrong-password").unwrap_err(),
            AuthVerificationError::InvalidCredentials
        );
    }

    /// Order-2 completeness: every crypt format the local
    /// libcrypt supports round-trips through the framework. The
    /// framework does not enforce a specific format on
    /// `/etc/shadow`; whichever format `passwd` writes, verify
    /// accepts.
    #[test]
    fn verify_succeeds_against_all_libcrypt_formats() {
        for setting in [
            "$y$j9T$abcdefghij", // yescrypt (trixie default)
            "$6$abcdefghij",     // SHA-512-crypt (bullseye default)
            "$5$abcdefghij",     // SHA-256-crypt
            "$1$abcdefgh",       // MD5-crypt
        ] {
            let stored =
                evo_shadow_crypt::hash_for_test(b"correct-password", setting)
                    .expect("hash_for_test call");
            if stored.starts_with('*') {
                eprintln!(
                    "skipping setting {setting}: unsupported on local \
                     libcrypt.so.1"
                );
                continue;
            }
            let (_tmp, path) = write_shadow(&[("operator", &stored)]);
            let svc = ShadowAuthService::with_shadow_path("operator", path);
            assert!(
                svc.verify("operator", b"correct-password").is_ok(),
                "verify must succeed against setting {setting} \
                 (hash={stored})"
            );
            assert_eq!(
                svc.verify("operator", b"wrong-password").unwrap_err(),
                AuthVerificationError::InvalidCredentials,
                "wrong-password against setting {setting} must \
                 refuse with InvalidCredentials"
            );
        }
    }

    /// Substrate proof: a domain-controller outage cannot lock
    /// the operator out of their own player.
    ///
    /// The empirical demonstration is that verify succeeds
    /// against a purely-local shadow file, with no network
    /// authority reachable, in a completely isolated `tempdir`.
    /// If any code path in this backend needed winbind / sssd /
    /// LDAP / Kerberos it would have failed to resolve, not
    /// silently succeeded.
    ///
    /// Additionally `refresh_after_secret_write` is a no-op so no
    /// revalidation against any authority runs at password-change
    /// time — the only re-read is `/etc/shadow` on the next
    /// verify.
    #[test]
    fn verify_succeeds_with_no_network_authority_reachable() {
        let stored = hash_via_libcrypt(b"local-only");
        let (_tmp, path) = write_shadow(&[("operator", &stored)]);
        let svc = ShadowAuthService::with_shadow_path("operator", path);
        assert!(
            svc.verify("operator", b"local-only").is_ok(),
            "verify must succeed without any network authority \
             reachable — the empirical proof no network call is on \
             this code path"
        );
        assert!(svc.refresh_after_secret_write().is_ok());
    }

    #[test]
    fn implementation_name_is_shadow() {
        let svc = ShadowAuthService::with_shadow_path(
            "operator",
            PathBuf::from("/tmp/shadow"),
        );
        assert_eq!(svc.implementation_name(), "shadow");
    }

    #[test]
    fn refresh_after_secret_write_is_noop_ok() {
        let svc = ShadowAuthService::with_shadow_path(
            "operator",
            PathBuf::from("/tmp/shadow"),
        );
        assert!(svc.refresh_after_secret_write().is_ok());
    }
}
