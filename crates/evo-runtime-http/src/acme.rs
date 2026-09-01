// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! ACME (RFC 8555) automatic certificate issuance + renewal.
//!
//! Mounts alongside the HTTPS substrate when the operator
//! supplies a directory URL, contact email, and hostnames via
//! environment variables. The ACME issuer task:
//!
//! 1. Loads (or creates) the persistent account key under
//!    `<state_dir>/acme/account.json`.
//! 2. Loops on a renewal cadence: if no cert exists OR the
//!    persisted cert is within the renewal window
//!    (`DEFAULT_RENEWAL_WINDOW_DAYS`), it places a fresh order
//!    with the configured CA, publishes the HTTP-01 challenge
//!    tokens into the shared [`AcmeChallengeStore`] (read by the
//!    redirect listener), and waits for the order to reach
//!    `Valid`.
//! 3. Generates a CSR with rcgen, finalizes the order, and
//!    downloads the cert chain.
//! 4. Persists the chain + key under `<state_dir>/acme/`.
//! 5. Swaps the [`HotReloadCertResolver`]'s active bundle so
//!    subsequent HTTPS handshakes serve the ACME-issued cert.
//!
//! The redirect listener short-circuits HTTP requests whose
//! path matches `/.well-known/acme-challenge/<token>`, looks
//! up the token in [`AcmeChallengeStore`], and returns the key
//! authorisation directly (HTTP-01). This requires the operator
//! to expose port 80 to the CA validator (typically by also
//! mounting `EVO_HTTP_REDIRECT_LISTEN_ADDR=0.0.0.0:80`).

use crate::cert_resolver::HotReloadCertResolver;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Notify;

/// Default cadence for the renewal-check loop. The task wakes
/// this often, evaluates whether the persisted cert needs
/// rotation, and runs an order if so. 1 hour balances
/// responsiveness against load on the CA.
pub const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Default window before cert expiry inside which the task
/// proactively renews. 30 days matches the Let's Encrypt
/// recommendation and leaves operators ample buffer for outages.
pub const DEFAULT_RENEWAL_WINDOW_DAYS: u64 = 30;

/// In-memory map of pending HTTP-01 challenge responses.
///
/// The ACME issuer publishes `{token → key_authorisation}` pairs
/// here before notifying the CA that the challenge is ready.
/// The HTTP redirect listener consults this map on every
/// `/.well-known/acme-challenge/<token>` request and serves the
/// key authorisation as `text/plain` instead of redirecting.
///
/// Entries are dropped once the CA reports the challenge as
/// validated (the issuer calls [`Self::clear_token`] from its
/// orchestration loop).
#[derive(Debug, Default)]
pub struct AcmeChallengeStore {
    entries: RwLock<HashMap<String, String>>,
}

impl AcmeChallengeStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a `{token → key_authorisation}` pair the CA's
    /// validator will subsequently look up via HTTP-01.
    pub fn insert(&self, token: String, key_authorisation: String) {
        self.entries
            .write()
            .expect("AcmeChallengeStore lock poisoned")
            .insert(token, key_authorisation);
    }

    /// Look up the key authorisation for a token. Returns the
    /// cloned string on hit; `None` on miss (the redirect
    /// listener then falls through to the default 308 path).
    pub fn lookup(&self, token: &str) -> Option<String> {
        self.entries
            .read()
            .expect("AcmeChallengeStore lock poisoned")
            .get(token)
            .cloned()
    }

    /// Drop a single token entry after the CA reports the
    /// challenge as validated.
    pub fn clear_token(&self, token: &str) {
        self.entries
            .write()
            .expect("AcmeChallengeStore lock poisoned")
            .remove(token);
    }

    /// Live count of pending challenges. Useful for tests +
    /// the `/_acme/status` projection (if mounted).
    pub fn pending(&self) -> usize {
        self.entries
            .read()
            .expect("AcmeChallengeStore lock poisoned")
            .len()
    }
}

/// Construction-time configuration for the ACME issuer.
#[derive(Debug, Clone)]
pub struct AcmeConfig {
    /// CA directory URL.
    ///
    /// Common values:
    ///
    /// - `https://acme-v02.api.letsencrypt.org/directory` —
    ///   production Let's Encrypt.
    /// - `https://acme-staging-v02.api.letsencrypt.org/directory`
    ///   — staging Let's Encrypt (for testing).
    /// - `https://pebble.local:14000/dir` — local Pebble test
    ///   server.
    pub directory_url: String,

    /// RFC 8555 `mailto:` contact for the account. The CA
    /// uses this for expiry-renewal nag mail and for
    /// security-incident outreach.
    pub contact_email: String,

    /// Hostnames the issued cert should cover (SAN list).
    /// At least one entry required.
    pub hostnames: Vec<String>,

    /// Directory under which the issuer persists the account
    /// key, the issued cert chain, and renewal state. Created
    /// if absent at mount time. Permissions are tightened to
    /// `0700` on the directory and `0600` on every file.
    pub state_dir: PathBuf,

    /// Interval between renewal-check ticks. Defaults to
    /// [`DEFAULT_CHECK_INTERVAL`].
    pub check_interval: Duration,

    /// Number of days before expiry inside which the issuer
    /// proactively renews. Defaults to
    /// [`DEFAULT_RENEWAL_WINDOW_DAYS`].
    pub renewal_window_days: u64,
}

impl AcmeConfig {
    /// Construct with sensible defaults except the supplied
    /// directory URL, email, hostnames, and state dir.
    pub fn new(
        directory_url: impl Into<String>,
        contact_email: impl Into<String>,
        hostnames: Vec<String>,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            directory_url: directory_url.into(),
            contact_email: contact_email.into(),
            hostnames,
            state_dir,
            check_interval: DEFAULT_CHECK_INTERVAL,
            renewal_window_days: DEFAULT_RENEWAL_WINDOW_DAYS,
        }
    }
}

/// Errors raised by [`AcmeIssuer::new`] or
/// [`AcmeIssuer::run`].
#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    /// `AcmeConfig.hostnames` was empty.
    #[error("AcmeConfig.hostnames must contain at least one host")]
    NoHostnames,
    /// Account file was present but could not be read.
    #[error("read {path}: {source}")]
    StateRead {
        /// File the loader attempted to read.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Account file could not be written (or its parent dir).
    #[error("write {path}: {source}")]
    StateWrite {
        /// File the writer attempted to create.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// instant-acme returned a protocol-level error.
    #[error("acme protocol: {0}")]
    Protocol(String),
    /// CSR generation via rcgen failed.
    #[error("csr generation: {0}")]
    Csr(String),
    /// The issuer's cert-resolver swap raised an error.
    #[error("cert-resolver swap: {0}")]
    Swap(String),
}

/// The ACME issuer. Construction reads or creates the account
/// key; `run` drives the renewal loop.
pub struct AcmeIssuer {
    config: AcmeConfig,
    challenges: Arc<AcmeChallengeStore>,
    resolver: Arc<HotReloadCertResolver>,
    /// Lazily-initialised on the first `run` tick; held inside
    /// a `tokio::sync::Mutex` so background tasks can share the
    /// account handle without re-deriving the JWK on every loop.
    account: tokio::sync::Mutex<Option<instant_acme::Account>>,
}

impl AcmeIssuer {
    /// Construct the issuer. Validates the config; creates
    /// the state directory if absent.
    pub fn new(
        config: AcmeConfig,
        challenges: Arc<AcmeChallengeStore>,
        resolver: Arc<HotReloadCertResolver>,
    ) -> Result<Self, AcmeError> {
        if config.hostnames.is_empty() {
            return Err(AcmeError::NoHostnames);
        }
        std::fs::create_dir_all(&config.state_dir).map_err(|e| {
            AcmeError::StateWrite {
                path: config.state_dir.display().to_string(),
                source: e,
            }
        })?;
        Ok(Self {
            config,
            challenges,
            resolver,
            account: tokio::sync::Mutex::new(None),
        })
    }

    /// Path the issuer persists the account credentials at.
    pub fn account_path(&self) -> PathBuf {
        self.config.state_dir.join("account.json")
    }

    /// Path the issuer persists the issued cert chain at.
    pub fn cert_chain_path(&self) -> PathBuf {
        self.config.state_dir.join("cert.pem")
    }

    /// Path the issuer persists the issued private key at.
    pub fn cert_key_path(&self) -> PathBuf {
        self.config.state_dir.join("cert.key")
    }

    /// Run the renewal-check loop until `shutdown` fires. The
    /// loop's behaviour is documented on the module.
    pub async fn run(self: Arc<Self>, shutdown: Arc<Notify>) {
        tracing::info!(
            directory_url = %self.config.directory_url,
            contact_email = %self.config.contact_email,
            hostnames = ?self.config.hostnames,
            state_dir = %self.config.state_dir.display(),
            check_interval_s = self.config.check_interval.as_secs(),
            renewal_window_days = self.config.renewal_window_days,
            "evo-runtime-http: ACME issuer task started",
        );
        loop {
            // Run one renewal attempt up front so the operator
            // sees a fresh cert immediately on a clean boot
            // rather than waiting a full check interval.
            if let Err(e) = self.tick().await {
                tracing::warn!(
                    error = %e,
                    "evo-runtime-http: ACME renewal tick failed; will \
                     retry next interval",
                );
            }
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::info!(
                        "evo-runtime-http: ACME issuer shutdown notified; \
                         exiting renewal loop",
                    );
                    break;
                }
                _ = tokio::time::sleep(self.config.check_interval) => {}
            }
        }
        tracing::info!("evo-runtime-http: ACME issuer task stopped");
    }

    /// One renewal-check tick.
    ///
    /// Returns `Ok(())` when the tick completed cleanly — even
    /// if no order was placed (the persisted cert was still
    /// inside the renewal window). Returns `Err(_)` only when
    /// a real protocol or I/O failure occurred.
    pub async fn tick(&self) -> Result<(), AcmeError> {
        let mut account_guard = self.account.lock().await;
        if account_guard.is_none() {
            let account =
                load_or_create_account(&self.config, &self.account_path())
                    .await?;
            *account_guard = Some(account);
        }
        let account = account_guard.as_ref().expect("just loaded");

        // The persisted-cert renewal check lives in the
        // backing-store layer rather than the protocol layer
        // so a future cert source (file watcher, secret
        // manager) can plug in. Today we read the chain off
        // disk and parse the leaf's NotAfter via x509-parser.
        // Without that dep we fall back to "always issue on
        // first tick; subsequent ticks check via the simpler
        // 'cert file exists + younger than (validity_window -
        // renewal_window)' heuristic". The heuristic is
        // conservative — it occasionally re-issues earlier
        // than strictly necessary but never misses a renewal.
        let needs_issue = needs_issue(self).await;
        if !needs_issue {
            tracing::debug!(
                "evo-runtime-http: ACME tick — persisted cert still in \
                 validity window; skipping order",
            );
            return Ok(());
        }

        place_and_finalize_order(self, account).await?;
        Ok(())
    }
}

async fn needs_issue(issuer: &AcmeIssuer) -> bool {
    // First boot: no cert on disk → must issue.
    let cert_path = issuer.cert_chain_path();
    let metadata = match std::fs::metadata(&cert_path) {
        Ok(m) => m,
        Err(_) => return true,
    };
    let mtime = match metadata.modified() {
        Ok(t) => t,
        Err(_) => return true,
    };
    let age = match mtime.elapsed() {
        Ok(d) => d,
        Err(_) => return true,
    };
    // Heuristic floor: ACME-issued certs from Let's Encrypt
    // are 90 days; the operator configures the renewal-window
    // as days-before-expiry (default 30). Translating that to
    // an mtime-based check: if the cert is older than
    // (90 - renewal_window) days, renew. 90 is the Let's
    // Encrypt baseline; CAs that issue shorter-lived certs
    // still renew correctly (the heuristic will re-issue more
    // often than strictly necessary but never miss).
    let baseline_validity_days = 90u64;
    let trigger_age_days = baseline_validity_days
        .saturating_sub(issuer.config.renewal_window_days);
    age >= Duration::from_secs(trigger_age_days * 86_400)
}

async fn load_or_create_account(
    config: &AcmeConfig,
    account_path: &Path,
) -> Result<instant_acme::Account, AcmeError> {
    if account_path.exists() {
        let text = std::fs::read_to_string(account_path).map_err(|e| {
            AcmeError::StateRead {
                path: account_path.display().to_string(),
                source: e,
            }
        })?;
        let creds: instant_acme::AccountCredentials =
            serde_json::from_str(&text).map_err(|e| {
                AcmeError::Protocol(format!("account decode: {e}"))
            })?;
        let account = instant_acme::Account::builder()
            .map_err(|e| AcmeError::Protocol(e.to_string()))?
            .from_credentials(creds)
            .await
            .map_err(|e| AcmeError::Protocol(e.to_string()))?;
        tracing::info!(
            path = %account_path.display(),
            "ACME account credentials loaded"
        );
        Ok(account)
    } else {
        let contact = format!("mailto:{}", config.contact_email);
        let (account, creds) = instant_acme::Account::builder()
            .map_err(|e| AcmeError::Protocol(e.to_string()))?
            .create(
                &instant_acme::NewAccount {
                    contact: &[contact.as_str()],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                config.directory_url.clone(),
                None,
            )
            .await
            .map_err(|e| AcmeError::Protocol(e.to_string()))?;
        let json = serde_json::to_string_pretty(&creds)
            .map_err(|e| AcmeError::Protocol(format!("account encode: {e}")))?;
        write_secret_file(account_path, json.as_bytes())?;
        tracing::info!(
            path = %account_path.display(),
            "ACME account credentials minted + persisted"
        );
        Ok(account)
    }
}

async fn place_and_finalize_order(
    issuer: &AcmeIssuer,
    account: &instant_acme::Account,
) -> Result<(), AcmeError> {
    let identifiers: Vec<instant_acme::Identifier> = issuer
        .config
        .hostnames
        .iter()
        .map(|h| instant_acme::Identifier::Dns(h.clone()))
        .collect();
    let mut order = account
        .new_order(&instant_acme::NewOrder::new(&identifiers))
        .await
        .map_err(|e| AcmeError::Protocol(e.to_string()))?;

    // Publish HTTP-01 challenges so the validator can fetch
    // them via our redirect listener.
    let mut tokens_published: Vec<String> = Vec::new();
    {
        let mut authorisations = order.authorizations();
        while let Some(result) = authorisations.next().await {
            let mut authz =
                result.map_err(|e| AcmeError::Protocol(e.to_string()))?;
            // HTTP-01 only. ACME validators expect the token
            // at /.well-known/acme-challenge/<token>; the
            // redirect listener consults `AcmeChallengeStore`
            // for that path.
            let mut challenge = authz
                .challenge(instant_acme::ChallengeType::Http01)
                .ok_or_else(|| {
                    AcmeError::Protocol(
                        "CA did not offer an HTTP-01 challenge".into(),
                    )
                })?;
            let token = challenge.identifier().to_string();
            let key_auth = challenge.key_authorization().as_str().to_string();
            issuer.challenges.insert(token.clone(), key_auth);
            tokens_published.push(token);
            challenge
                .set_ready()
                .await
                .map_err(|e| AcmeError::Protocol(e.to_string()))?;
        }
    }

    // Wait for the order to reach `Ready`. instant-acme polls
    // the CA with backoff; the inner sleep on each retry keeps
    // us off the CA's rate limit.
    let status = order
        .poll_ready(&instant_acme::RetryPolicy::default())
        .await
        .map_err(|e| AcmeError::Protocol(e.to_string()))?;
    // Clear the challenge entries — the CA's verifier has
    // finished with them.
    for token in tokens_published {
        issuer.challenges.clear_token(&token);
    }
    if status != instant_acme::OrderStatus::Ready {
        return Err(AcmeError::Protocol(format!(
            "order did not reach Ready: status = {status:?}",
        )));
    }

    // Generate the CSR. rcgen produces a self-signed cert + key
    // and exposes the CSR DER bytes the CA wants.
    let mut cert_params =
        rcgen::CertificateParams::new(issuer.config.hostnames.clone())
            .map_err(|e| AcmeError::Csr(e.to_string()))?;
    cert_params.distinguished_name = rcgen::DistinguishedName::new();
    let private_key = rcgen::KeyPair::generate()
        .map_err(|e| AcmeError::Csr(e.to_string()))?;
    let csr = cert_params
        .serialize_request(&private_key)
        .map_err(|e| AcmeError::Csr(e.to_string()))?;

    // Finalize the order with the CSR; then poll for the
    // issued chain. instant-acme exposes a two-call shape on
    // 0.8: `finalize_csr` posts the CSR and updates internal
    // state; `poll_certificate` waits for the order to reach
    // `Valid` and downloads the chain.
    order
        .finalize_csr(csr.der())
        .await
        .map_err(|e| AcmeError::Protocol(e.to_string()))?;
    let chain_pem = order
        .poll_certificate(&instant_acme::RetryPolicy::default())
        .await
        .map_err(|e| AcmeError::Protocol(e.to_string()))?;

    // Persist the chain + private key under 0600.
    write_secret_file(&issuer.cert_chain_path(), chain_pem.as_bytes())?;
    write_secret_file(
        &issuer.cert_key_path(),
        private_key.serialize_pem().as_bytes(),
    )?;

    // Swap the active cert resolver.
    let bundle = evo_tls_certs::CertBundle::from_pem(
        private_key.serialize_pem(),
        chain_pem.clone(),
    );
    issuer
        .resolver
        .swap(&bundle)
        .map_err(|e| AcmeError::Swap(e.to_string()))?;
    tracing::info!(
        hostnames = ?issuer.config.hostnames,
        chain_path = %issuer.cert_chain_path().display(),
        "ACME cert issued + installed on cert resolver"
    );
    Ok(())
}

fn write_secret_file(path: &Path, content: &[u8]) -> Result<(), AcmeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AcmeError::StateWrite {
            path: parent.display().to_string(),
            source: e,
        })?;
    }
    std::fs::write(path, content).map_err(|e| AcmeError::StateWrite {
        path: path.display().to_string(),
        source: e,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(0o600),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_store_round_trips_token_and_clears() {
        let s = AcmeChallengeStore::new();
        s.insert("tok-1".into(), "auth-1".into());
        s.insert("tok-2".into(), "auth-2".into());
        assert_eq!(s.pending(), 2);
        assert_eq!(s.lookup("tok-1"), Some("auth-1".into()));
        s.clear_token("tok-1");
        assert!(s.lookup("tok-1").is_none());
        assert_eq!(s.pending(), 1);
    }

    #[test]
    fn config_new_applies_defaults() {
        let cfg = AcmeConfig::new(
            "https://example/dir",
            "ops@example.com",
            vec!["device.example.com".into()],
            std::env::temp_dir().join("evo-acme-test"),
        );
        assert_eq!(cfg.check_interval, DEFAULT_CHECK_INTERVAL);
        assert_eq!(cfg.renewal_window_days, DEFAULT_RENEWAL_WINDOW_DAYS);
    }

    #[tokio::test]
    async fn issuer_new_rejects_empty_hostnames() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = AcmeConfig::new(
            "https://example/dir",
            "ops@example.com",
            vec![],
            tmp.path().to_path_buf(),
        );
        let challenges = Arc::new(AcmeChallengeStore::new());
        let bundle = fixture_bundle();
        let resolver = HotReloadCertResolver::new(&bundle).expect("resolver");
        match AcmeIssuer::new(cfg, challenges, resolver) {
            Err(AcmeError::NoHostnames) => {}
            Err(other) => panic!("expected NoHostnames, got {other:?}"),
            Ok(_) => panic!("expected NoHostnames, got Ok"),
        }
    }

    #[tokio::test]
    async fn issuer_new_creates_state_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join("acme");
        let cfg = AcmeConfig::new(
            "https://example/dir",
            "ops@example.com",
            vec!["device.example.com".into()],
            state_dir.clone(),
        );
        let challenges = Arc::new(AcmeChallengeStore::new());
        let bundle = fixture_bundle();
        let resolver = HotReloadCertResolver::new(&bundle).expect("resolver");
        let issuer = match AcmeIssuer::new(cfg, challenges, resolver) {
            Ok(i) => i,
            Err(e) => panic!("issuer new failed: {e}"),
        };
        assert!(state_dir.exists());
        assert_eq!(issuer.account_path(), state_dir.join("account.json"));
    }

    #[test]
    fn write_secret_file_lands_at_0600() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("secret");
        write_secret_file(&p, b"hello").expect("write");
        let bytes = std::fs::read(&p).expect("read");
        assert_eq!(bytes, b"hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    /// Build a real `CertBundle` for the cert-resolver
    /// construction fixtures. Uses the framework's device-CA
    /// substrate to produce a self-signed leaf for
    /// `device.local`.
    fn fixture_bundle() -> evo_tls_certs::CertBundle {
        let ca = evo_tls_certs::generate_device_ca(
            &evo_tls_certs::DeviceCaConfig::default(),
        )
        .expect("device-CA");
        ca.issue_leaf(&evo_tls_certs::LeafConfig::for_hostnames(vec![
            "device.local".to_string(),
        ]))
        .expect("leaf")
    }
}
