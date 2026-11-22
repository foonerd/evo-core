// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! HTTPS boot wiring.
//!
//! [`boot_https`] composes every substrate built in this
//! workspace — the device-CA cert lifecycle, the bearer-
//! token validator, the witness chain, the observability
//! ring, the auto-rotator, and the canonical schema —
//! into a single running HTTPS listener bound alongside
//! the steward's UDS socket. The same trust boundary that
//! admits a request explains and audits it: every wire op,
//! every cert event, every token decision lands in the
//! Owl; every privileged dispatch produces a witness in
//! the cryptographically-chained ledger.
//!
//! The boot is idempotent on subsequent runs: long-lived
//! material (device-CA PEMs, token signing key, witness
//! signing key, operator bootstrap token) persists under
//! the steward state directory and reloads on every boot.
//! Short-lived material (the leaf cert) is regenerated at
//! every boot and refreshed continuously by the
//! [`evo_tls_certs::AutoRotator`].

use crate::http_dispatcher::StewardHttpDispatcher;
use crate::projection_schema::canonical_schema;
use crate::server::Server;
use evo_auth_bearer::{
    BearerTokenIssuer, BearerTokenValidator, Capability, CapabilitySet,
    CredentialStore,
};
use evo_observatory::{Observatory, ObservatoryConfig};
use evo_runtime_http::{
    build_router, install_auto_rotation, serve_https, AuthTier,
    AuthTierProvider, HttpsListenerConfig, NoopAuditSink, ServerHandle,
};
use evo_tls_certs::{
    generate_device_ca, CertLifecycle, CertRotationPolicy, DeviceCaConfig,
    GeneratedCa, LeafConfig, RotationStats,
};
use evo_witness::WitnessChain;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

/// Configuration for the HTTPS boot.
///
/// `Debug` is implemented manually because the
/// `asset_cache` field holds an `Arc<dyn AssetCache>` trait
/// object that does not itself implement `Debug`; the impl
/// renders the field as `<set>` / `<unset>` instead.
#[derive(Clone)]
pub struct HttpsBootConfig {
    /// Socket the listener binds to.
    pub listen_addr: SocketAddr,
    /// Hostnames + IPs the leaf cert's SAN advertises.
    /// `localhost` and `127.0.0.1` are typical for a
    /// device that has no routable DNS name.
    pub hostnames: Vec<String>,
    /// Steward state directory. The boot helper writes
    /// long-lived material under
    /// `state_dir/https/{ca.crt,ca.key,tokens.key,witness.key,bootstrap.token}`.
    pub state_dir: PathBuf,
    /// Common name on the device-CA. Surfaces on the cert
    /// when an operator inspects it manually.
    pub ca_common_name: String,
    /// Auto-rotation cadence for the leaf cert. Default 30
    /// days; under leaf TTL of 90 days, the leaf rotates 3
    /// times per lifetime.
    pub rotation_policy: CertRotationPolicy,
    /// Observatory ring capacity. Default 16384.
    pub observatory_capacity: usize,
    /// Optional PEM file path holding one or more trusted
    /// client-CA certs. When set the HTTPS listener requires
    /// every client to present a TLS cert that chains to one
    /// of the listed CAs; clients without a cert (or with an
    /// untrusted one) are refused at the TLS layer before any
    /// HTTP request is decoded. Unset = no client-cert
    /// verification (the framework default).
    pub client_ca_pem_path: Option<PathBuf>,
    /// Optional content-addressed asset cache. The framework
    /// stores and retrieves bytes by hash and mounts no route
    /// over them. When supplied it is handed to plugins
    /// through their load context, and to the HTTPS hookup so
    /// a distribution can mount its own serving surface.
    /// Absent, the store is simply unavailable; the
    /// schema-driven routes + WS endpoint + observatory +
    /// witness endpoints are unaffected.
    pub asset_cache:
        Option<Arc<dyn evo_plugin_sdk::contract::asset_cache::AssetCache>>,
    /// Optional source for the operator-selected auth tier.
    /// Defaults to the env-var-driven static provider
    /// ([`evo_runtime_http::default_auth_tier_provider`]),
    /// which yields `AuthTier::Open` when `EVO_AUTH_TIER` is
    /// unset. The operator opens the browser on the LAN
    /// under Open and works without credential ceremony.
    pub auth_tier_provider: Option<Arc<dyn evo_runtime_http::AuthTierProvider>>,
    /// Optional bound on what a caller admitted without a bearer
    /// may hold. Without it the no-bearer arms use
    /// [`lan_trust_capability_set`] (the floor) and, for a LAN
    /// origin, [`Self::lan_privileged_capabilities`]. The floor is
    /// deliberate: LAN-trust is broad-audience (any browser on
    /// the LAN is admitted without a bearer under the Open /
    /// Secure-LAN tiers) and a single-holder role composes
    /// catastrophically with broad-audience admission — any
    /// device on the LAN would race the operator's actual UI
    /// session for the role.
    ///
    /// The bound narrows and never widens: each no-bearer set
    /// keeps only the scopes the bound also names, at the lower
    /// of the two ranks (see [`narrow_capability_set`]). One
    /// capability survives any bound: `write:pair`
    /// ([`PAIRING_SCOPE`]), because pairing is how a caller with
    /// no bearer earns one. An empty bound therefore leaves a
    /// no-bearer caller holding pairing, the capability-none ops,
    /// and nothing else. A distribution sets it through
    /// [`crate::RunOptions::with_no_bearer_capabilities`].
    pub lan_trust_capabilities: Option<evo_auth_bearer::CapabilitySet>,
    /// LAN-origin privileged set, offered only when the steward
    /// carries a household-protection runtime. `None` keeps the LAN
    /// arm on the playback floor. See
    /// [`lan_privileged_capability_set`].
    pub lan_privileged_capabilities: Option<evo_auth_bearer::CapabilitySet>,
    /// How long an accepted connection has to complete its TLS
    /// handshake and send the first bytes of a request before the
    /// listener closes it. Default
    /// [`evo_runtime_http::config::DEFAULT_HANDSHAKE_TIMEOUT`];
    /// `EVO_HTTPS_HANDSHAKE_TIMEOUT_SECS` overrides it on the steward.
    pub handshake_timeout: Duration,
    /// How many accepted connections may be waiting for their first
    /// request bytes at once; past it, the listener accepts nothing
    /// more until one completes, fails, or runs out of time. Default
    /// [`evo_runtime_http::config::DEFAULT_MAX_INCOMPLETE_CONNECTIONS`];
    /// `EVO_HTTPS_MAX_INCOMPLETE_CONNECTIONS` overrides it on the
    /// steward.
    pub max_incomplete_connections: NonZeroUsize,
}

impl std::fmt::Debug for HttpsBootConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsBootConfig")
            .field("listen_addr", &self.listen_addr)
            .field("hostnames", &self.hostnames)
            .field("state_dir", &self.state_dir)
            .field("ca_common_name", &self.ca_common_name)
            .field("rotation_policy", &self.rotation_policy)
            .field("observatory_capacity", &self.observatory_capacity)
            .field("client_ca_pem_path", &self.client_ca_pem_path)
            .field(
                "asset_cache",
                &self
                    .asset_cache
                    .as_ref()
                    .map(|_| "<set>")
                    .unwrap_or("<unset>"),
            )
            .field(
                "auth_tier_provider",
                &self
                    .auth_tier_provider
                    .as_ref()
                    .map(|_| "<set>")
                    .unwrap_or("<unset>"),
            )
            .field(
                "lan_trust_capabilities",
                &self
                    .lan_trust_capabilities
                    .as_ref()
                    .map(|_| "<set>")
                    .unwrap_or("<unset>"),
            )
            .field("handshake_timeout", &self.handshake_timeout)
            .field(
                "max_incomplete_connections",
                &self.max_incomplete_connections,
            )
            .finish()
    }
}

impl HttpsBootConfig {
    /// Sensible defaults for a device that binds on the
    /// supplied socket.
    pub fn new(listen_addr: SocketAddr, state_dir: PathBuf) -> Self {
        Self {
            listen_addr,
            hostnames: vec!["localhost".to_string(), "127.0.0.1".to_string()],
            state_dir,
            ca_common_name: "evo device-CA".to_string(),
            rotation_policy: CertRotationPolicy::default(),
            observatory_capacity: 16_384,
            client_ca_pem_path: None,
            asset_cache: None,
            auth_tier_provider: None,
            lan_trust_capabilities: None,
            lan_privileged_capabilities: None,
            handshake_timeout:
                evo_runtime_http::config::DEFAULT_HANDSHAKE_TIMEOUT,
            max_incomplete_connections:
                evo_runtime_http::config::DEFAULT_MAX_INCOMPLETE_CONNECTIONS,
        }
    }

    /// Take the listener's connection bounds from the environment:
    /// `EVO_HTTPS_HANDSHAKE_TIMEOUT_SECS` and
    /// `EVO_HTTPS_MAX_INCOMPLETE_CONNECTIONS`, each a whole number
    /// greater than zero. An unset variable leaves its bound as it
    /// was; any other value that is not such a number is an error
    /// naming the variable.
    pub fn with_listener_bounds_from_env(mut self) -> anyhow::Result<Self> {
        if let Some(secs) =
            positive_from_env("EVO_HTTPS_HANDSHAKE_TIMEOUT_SECS")?
        {
            self.handshake_timeout = Duration::from_secs(secs.get());
        }
        const BOUND: &str = "EVO_HTTPS_MAX_INCOMPLETE_CONNECTIONS";
        if let Some(bound) = positive_from_env(BOUND)? {
            self.max_incomplete_connections = NonZeroUsize::try_from(bound)
                .map_err(|_| {
                    anyhow::anyhow!(
                        "{BOUND}={bound} is more than this platform can count"
                    )
                })?;
        }
        Ok(self)
    }
}

/// The whole number greater than zero the environment variable `name`
/// holds; `None` when it is unset.
fn positive_from_env(name: &str) -> anyhow::Result<Option<NonZeroU64>> {
    parse_positive(name, std::env::var_os(name).as_deref())
}

/// `raw` read as a whole number greater than zero; `None` when absent.
/// Anything else is an error naming `name`.
fn parse_positive(
    name: &str,
    raw: Option<&OsStr>,
) -> anyhow::Result<Option<NonZeroU64>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let text = raw.to_string_lossy();
    text.parse::<NonZeroU64>().map(Some).map_err(|e| {
        anyhow::anyhow!(
            "{name}={text:?} is not a whole number greater than zero: {e}"
        )
    })
}

/// Handles produced by a successful boot.
///
/// The caller awaits `listener_task` to detect shutdown;
/// the rotator runs alongside until `shutdown` fires. The
/// bootstrap operator token (one-time, issued on first
/// boot only) is surfaced so the host can log it for the
/// operator to capture.
pub struct HttpsBootHandles {
    /// Listener handle including the local bound address +
    /// the shutdown notifier the caller fires on exit.
    pub server: ServerHandle,
    /// Listener task — `await` after firing shutdown.
    pub listener_task: JoinHandle<()>,
    /// Rotator task — `await` after firing shutdown.
    pub rotator_task: JoinHandle<RotationStats>,
    /// Owl observatory. Operators inspect via
    /// `/_observatory/*`; the caller may also read
    /// directly via this handle.
    pub observatory: Arc<Observatory>,
    /// Witness chain. Operators inspect via `/_witness/*`.
    pub witness_chain: Arc<WitnessChain>,
    /// Bearer-token validator. The caller can mint
    /// operator tokens by holding the matching issuer.
    pub validator: Arc<BearerTokenValidator>,
    /// Bearer-token issuer. The caller mints operator
    /// tokens through this surface; the verifying key half
    /// is shared with the validator. Wrapped in [`Arc`] so
    /// the steward-state handle bag can hold a sibling
    /// reference for the operator-mint wire-op path
    /// (`mint_bearer_token`) without pulling ownership away
    /// from the HTTPS substrate.
    pub issuer: Arc<BearerTokenIssuer>,
    /// Operator credential inventory. File-backed under
    /// `state_dir/https/credentials/`; each minted bearer
    /// token persists a [`evo_auth_bearer::CredentialRecord`]
    /// here so the operator can list, revoke, and audit
    /// credentials without depending on what mint-flow
    /// stdout output was captured.
    pub credential_store: Arc<evo_auth_bearer::CredentialStore>,
    /// Live revocation list backed by
    /// `state_dir/https/revoked.json`. Shared with the
    /// bearer-token validator (which consults it on every
    /// request) and with the revoke wire-op (which writes
    /// through).
    pub revocation_list: Arc<evo_auth_bearer::RevocationList>,
    /// The directory this boot loaded its material from:
    /// `state_dir/https/`. The revoke and reset wire ops write the
    /// revocation list here, where the next boot reads it.
    pub https_dir: PathBuf,
    /// The auth tier every admission path consults. The reset wire op
    /// holds it at `Open`.
    pub(crate) auth_tier: Arc<StewardAuthTier>,
    /// If this is the device's first boot, the helper
    /// minted a bootstrap operator token bearing every
    /// scope the framework exposes and persisted it under
    /// `state_dir/https/bootstrap.token`. Subsequent boots
    /// return `None` — the operator was expected to
    /// capture the token at first boot.
    pub bootstrap_token_b64: Option<String>,
}

/// Compose and spawn the HTTPS substrate.
///
/// At first boot the helper generates the device-CA, the
/// bearer-token signing key, the witness signing key, and
/// a bootstrap operator token, persisting them under
/// `state_dir/https/`. Subsequent boots reload the
/// persisted material. The leaf cert is regenerated at
/// every boot and refreshed continuously by the
/// auto-rotator.
///
/// Errors surface as `anyhow::Error` so the boot path can
/// fail-fast with operator-readable context.
pub async fn boot_https(
    server: Arc<Server>,
    config: HttpsBootConfig,
    https_setup: Option<crate::HttpsSetup>,
) -> anyhow::Result<HttpsBootHandles> {
    let https_dir = config.state_dir.join("https");
    tokio::fs::create_dir_all(&https_dir).await?;

    // 1. Device-CA: load-or-generate.
    let ca_cert_path = https_dir.join("ca.crt");
    let ca_key_path = https_dir.join("ca.key");
    let ca = if ca_cert_path.exists() && ca_key_path.exists() {
        let cert_pem = tokio::fs::read_to_string(&ca_cert_path).await?;
        let key_pem = tokio::fs::read_to_string(&ca_key_path).await?;
        GeneratedCa::from_pem(&cert_pem, &key_pem)?
    } else {
        let fresh = generate_device_ca(&DeviceCaConfig {
            common_name: config.ca_common_name.clone(),
            ttl_days: evo_tls_certs::device_ca::DEFAULT_CA_TTL_DAYS,
        })?;
        write_pem(&ca_cert_path, &fresh.ca_cert_pem).await?;
        write_pem_secret(&ca_key_path, &fresh.ca_key_pem).await?;
        fresh
    };
    let ca = Arc::new(ca);

    // 2. Bearer-token signing key: load-or-generate. A credentials
    //    reset retired the key it found; that key is replaced before
    //    anything loads it, or the boot stops here.
    let retiring_in = https_dir.clone();
    let retired = tokio::task::spawn_blocking(move || {
        replace_retired_token_key(&retiring_in)
    })
    .await??;
    if let Some(rotation) = retired {
        tracing::info!(
            records_cleared = rotation.records_cleared,
            revocations_cleared = rotation.revocations_cleared,
            bootstrap_token_removed = rotation.bootstrap_token_removed,
            "bearer-token signing key replaced after the credentials reset; \
             every bearer issued before the reset is refused"
        );
    }
    let token_key_path = https_dir.join("tokens.key");
    let token_signing_key = if token_key_path.exists() {
        let raw = tokio::fs::read(&token_key_path).await?;
        if raw.len() != 32 {
            anyhow::bail!("tokens.key is {} bytes; expected 32", raw.len());
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&raw);
        ed25519_dalek::SigningKey::from_bytes(&bytes)
    } else {
        let key = BearerTokenIssuer::generate_signing_key();
        write_raw_secret(&token_key_path, &key.to_bytes()).await?;
        key
    };

    // 3. Witness signing key: load-or-generate.
    let witness_key_path = https_dir.join("witness.key");
    let witness_signing_key = if witness_key_path.exists() {
        let raw = tokio::fs::read(&witness_key_path).await?;
        if raw.len() != 32 {
            anyhow::bail!("witness.key is {} bytes; expected 32", raw.len());
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&raw);
        ed25519_dalek::SigningKey::from_bytes(&bytes)
    } else {
        let key = WitnessChain::generate_signing_key();
        write_raw_secret(&witness_key_path, &key.to_bytes()).await?;
        key
    };

    // 4. Compose substrates.
    let observatory = Arc::new(Observatory::new(ObservatoryConfig {
        capacity: config.observatory_capacity,
    }));
    let lifecycle =
        Arc::new(CertLifecycle::with_observatory(Arc::clone(&observatory)));

    let verifying_key = token_signing_key.verifying_key();

    // Load the persisted revocation list — `revoked.json` is
    // appended to by the `revoke_bearer_token` wire op so
    // revocations survive steward restarts. First-boot has
    // no file; an empty list is the correct initial state.
    let revoked_path = https_dir.join("revoked.json");
    let revocations = Arc::new(load_revocation_list(&revoked_path).await?);

    let validator = Arc::new(
        BearerTokenValidator::new(verifying_key, Arc::clone(&revocations))
            .with_observatory(Arc::clone(&observatory)),
    );
    let issuer = Arc::new(
        BearerTokenIssuer::new(token_signing_key)
            .with_observatory(Arc::clone(&observatory)),
    );

    // Operator credential inventory. File-backed; survives
    // steward restarts. Each minted bearer token persists a
    // CredentialRecord here so the operator can list,
    // revoke, and audit credentials without depending on
    // what the mint-flow stdout output was captured to.
    let credentials_dir = https_dir.join("credentials");
    let credential_store =
        Arc::new(CredentialStore::new(credentials_dir).map_err(|e| {
            anyhow::anyhow!("credential store init failed: {e}")
        })?);
    // Records whose bearers have expired go at every boot; a record is
    // written for every bearer, and the kiosk and paired browsers are
    // issued one a day.
    if let Err(e) = credential_store.prune_expired(now_ms()) {
        // LOGGING.md §2: warn (recoverable anomaly — the expired records
        // stay listed until the next bearer is issued; their bearers are
        // refused regardless).
        tracing::warn!(
            error = %e,
            "dropping expired credential records at boot failed; they \
             stay listed until the next attempt"
        );
    }

    let witness_chain = Arc::new(WitnessChain::new(witness_signing_key));

    // Periodic audit-chain retention: collapse entries
    // older than the rolling retention window into a
    // signed roll-up summary. Keeps the in-memory ring
    // bounded over long uptimes and preserves chain-hash
    // continuity so verifiers walk the chain end-to-end
    // without disruption. The task lives for the lifetime
    // of this runtime; tokio shutdown cancels it.
    let _audit_prune_task = crate::witness_retention::spawn_periodic_prune(
        Arc::clone(&witness_chain),
        crate::witness_retention::DEFAULT_PRUNE_INTERVAL,
        crate::witness_retention::DEFAULT_RETENTION_WINDOW,
    );

    // 5. Issue the initial leaf and persist the
    //    bootstrap operator token if this is first boot.
    //    The bootstrap credential records itself in the
    //    operator inventory so it appears in
    //    `list_bearer_tokens` output and the operator can
    //    revoke + replace it via the standard flow. It carries
    //    what a pair mint carries: the operator bootstrap set and
    //    every scope the plugins admitted by now declare.
    let bootstrap_token_path = https_dir.join("bootstrap.token");
    let bootstrap_token_b64 = if !bootstrap_token_path.exists() {
        let now = now_ms();
        let capabilities =
            merged_operator_bootstrap_capability_set(server.plugin_router());
        let token = issuer
            .issue(
                capabilities.clone(),
                evo_auth_bearer::DEFAULT_TOKEN_TTL_MS,
                now,
            )
            .map_err(|e| {
                anyhow::anyhow!("bootstrap token issuance failed: {e}")
            })?;
        let encoded = token.encode();
        write_raw_secret(&bootstrap_token_path, encoded.as_bytes()).await?;
        let record = evo_auth_bearer::CredentialRecord {
            token_id: token.id.clone(),
            name: "bootstrap".to_string(),
            created_reason: "first-boot bootstrap credential".to_string(),
            scopes: capabilities
                .capabilities()
                .iter()
                .map(evo_auth_bearer::CapabilityRef::from)
                .collect(),
            expiry_policy: evo_auth_bearer::ExpiryPolicy::Seconds {
                value: evo_auth_bearer::DEFAULT_TOKEN_TTL_MS / 1000,
            },
            created_at_ms: now,
            expires_at_ms: Some(token.expires_at_ms),
            revoked_at_ms: None,
            revoked_reason: None,
            source: Some(evo_auth_bearer::CredentialSource::Bootstrap),
            paired_device_id: None,
        };
        if let Err(e) = credential_store.put(&record) {
            tracing::warn!(
                error = %e,
                token_id = %token.id,
                "persisting bootstrap credential record failed; \
                 mint stdout output is still authoritative"
            );
        }
        Some(encoded)
    } else {
        None
    };

    let leaf = lifecycle.issue_leaf(
        &ca,
        &LeafConfig::for_hostnames(config.hostnames.clone()),
    )?;

    // 6. Build the canonical-schema router. The framework reads
    //    the operator-selected auth tier via the configured
    //    provider (defaults to the env-var-driven static
    //    provider; `Open` is the default tier). LAN-trust
    //    admission injects the operator capability set when
    //    a request arrives without a bearer header and the
    //    tier admits.
    let dispatcher = StewardHttpDispatcher::new(Arc::clone(&server));
    // Retained for the distribution hookup below: `build_router`
    // consumes the dispatcher, and the hookup composes plugin calls
    // through the same one rather than constructing a second.
    let dispatcher_for_hookup: Arc<dyn evo_runtime_http::Dispatcher> =
        Arc::clone(&dispatcher) as Arc<dyn evo_runtime_http::Dispatcher>;
    // Same adapter implements both Dispatcher (request/response)
    // and SubscriptionDispatcher (Subscribe-frame streaming).
    // One canonical object; no parallel adapter.
    let subscription_dispatcher: Arc<
        dyn evo_runtime_http::SubscriptionDispatcher,
    > = Arc::clone(&dispatcher)
        as Arc<dyn evo_runtime_http::SubscriptionDispatcher>;
    let schema = canonical_schema();
    // The configured tier, held at `Open` while a credentials reset
    // says so. Every admission path holds this one provider.
    let open_after_reset =
        https_dir.join(OPEN_TIER_AFTER_RESET).try_exists()?;
    let auth_tier = Arc::new(StewardAuthTier::new(
        config
            .auth_tier_provider
            .clone()
            .unwrap_or_else(evo_runtime_http::default_auth_tier_provider),
        open_after_reset,
    ));
    let tier_provider: Arc<dyn AuthTierProvider> = auth_tier.clone();
    let (lan_trust_caps, lan_privileged_caps) = no_bearer_capability_sets(
        config.lan_trust_capabilities.as_ref(),
        config.lan_privileged_capabilities.as_ref(),
    );
    // `build_router` consumes the capability set, and hookup routes
    // must gate LAN-trusted callers identically.
    let lan_trust_caps_for_hookup = lan_trust_caps.clone();
    tracing::info!(
        tier = %tier_provider.current(),
        configured = %auth_tier.configured(),
        open_after_reset,
        "evo https listener: auth tier initialised",
    );
    let mut router = build_router(
        &schema,
        "/api/v1",
        dispatcher,
        subscription_dispatcher,
        Arc::clone(&validator),
        NoopAuditSink::shared(),
        Some(Arc::clone(&observatory)),
        Some(Arc::clone(&witness_chain)),
        Arc::clone(&tier_provider),
        lan_trust_caps,
        lan_privileged_caps,
    )?;

    // 6b. Optional static-asset serving. When the operator sets
    //     `EVO_HTTPS_STATIC_DIR` the framework's HTTPS substrate
    //     also serves the schema-first UI shell (or any vendor
    //     `ui_shell` bundle) directly from the same origin as the
    //     API. Same-origin keeps PWA scoping clean (manifest +
    //     service-worker scope `/`) and removes the need for a
    //     separate static-asset server alongside the steward.
    if let Some(static_dir) = std::env::var_os("EVO_HTTPS_STATIC_DIR") {
        let path = std::path::PathBuf::from(static_dir);
        if path.is_dir() {
            router = evo_runtime_http::attach_static_assets(router, &path);
            tracing::info!(
                static_dir = %path.display(),
                "evo https listener: static-asset fallback mounted at /",
            );
        } else {
            tracing::warn!(
                static_dir = %path.display(),
                "EVO_HTTPS_STATIC_DIR points at a path that does not exist or \
                 is not a directory; static-asset serving disabled",
            );
        }
    }

    // 6c. Distribution-supplied HTTPS hookup — the seam where
    //     product routes attach.
    //
    //     The framework has now mounted everything it owns:
    //     schema-driven wire ops, TLS, auth, observatory, witness,
    //     and the optional static-asset fallback. A distribution
    //     that ships product HTTP mounts it here, on a router it
    //     receives and returns, rather than having the framework's
    //     own router construction know which domain it is serving.
    //
    //     Absent on the shipped `evo` binary, which therefore
    //     mounts no product routes at all. The framework mints a
    //     claimant token for whatever actor name the hookup
    //     declares and invents none of its own.
    if let Some(hookup) = https_setup {
        let claimant_name = hookup.claimant_name.clone();
        tracing::info!(
            claimant = %claimant_name,
            "evo https listener: invoking distribution HTTPS hookup",
        );
        let ctx = crate::HttpsSetupContext {
            router,
            dispatcher: dispatcher_for_hookup,
            asset_cache: config.asset_cache.clone(),
            validator: Arc::clone(&validator),
            bus: server.happening_bus(),
            api_prefix: "/api/v1".to_string(),
            state_dir: config.state_dir.clone(),
            tier_provider: Arc::clone(&tier_provider),
            lan_trust_caps: lan_trust_caps_for_hookup,
            claimant_name,
        };
        router = (hookup.hook)(ctx).await.map_err(|e| {
            anyhow::anyhow!("distribution HTTPS hookup failed: {e}")
        })?;
    }

    // 7. Bind the HTTPS listener. When `client_ca_pem_path` is
    //    set the listener is mounted with mTLS so every TLS
    //    handshake is gated on the client presenting a cert
    //    that chains to one of the trusted CAs.
    let listener_cfg = HttpsListenerConfig::new(config.listen_addr)
        .with_handshake_timeout(config.handshake_timeout)
        .with_max_incomplete_connections(config.max_incomplete_connections);
    let (handle, listener_task) = match &config.client_ca_pem_path {
        Some(path) => {
            let verifier = evo_runtime_http::build_client_verifier(path)?;
            tracing::info!(
                path = %path.display(),
                "evo https listener: mTLS enabled; client certs required"
            );
            evo_runtime_http::serve_https_with_mtls(
                listener_cfg,
                router,
                &leaf,
                verifier,
            )
            .await?
        }
        None => serve_https(listener_cfg, router, &leaf).await?,
    };

    // 8. Spawn the auto-rotator.
    let rotator_task = install_auto_rotation(
        Arc::clone(&handle.cert_resolver),
        Arc::clone(&ca),
        LeafConfig::for_hostnames(config.hostnames.clone()),
        Arc::clone(&lifecycle),
        config.rotation_policy,
        Arc::clone(&handle.shutdown),
    );

    Ok(HttpsBootHandles {
        server: handle,
        listener_task,
        rotator_task,
        observatory,
        witness_chain,
        validator,
        issuer,
        credential_store,
        revocation_list: revocations,
        https_dir,
        auth_tier,
        bootstrap_token_b64,
    })
}

/// Load the persisted revocation list from
/// `<https_dir>/revoked.json`. The on-disk shape is a JSON
/// array of token id strings appended to by the
/// `revoke_bearer_token` wire op. Returns an empty list on
/// first boot (file absent) or when the file is malformed
/// (operator's revocations would survive even if the file
/// drifts; logging the drift is enough — refusing to boot
/// would lock the operator out for a recoverable hiccup).
async fn load_revocation_list(
    path: &Path,
) -> anyhow::Result<evo_auth_bearer::RevocationList> {
    use evo_auth_bearer::RevocationList;
    if !path.exists() {
        return Ok(RevocationList::new());
    }
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "revocation list read failed; starting with empty list"
            );
            return Ok(RevocationList::new());
        }
    };
    let ids: Vec<String> = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "revocation list parse failed; starting with empty list"
            );
            return Ok(RevocationList::new());
        }
    };
    let list = evo_auth_bearer::RevocationList::new();
    for id in &ids {
        list.revoke(id);
    }
    tracing::info!(
        path = %path.display(),
        count = ids.len(),
        "revocation list loaded"
    );
    Ok(list)
}

/// Append a token id to the persisted revocation list. Reads
/// the existing list (empty if absent), inserts the id, and
/// writes back atomically via tempfile-rename. Idempotent —
/// adding an id that is already present is a no-op on disk.
pub async fn persist_revocation(
    https_dir: &Path,
    token_id: &str,
) -> anyhow::Result<()> {
    let path = https_dir.join("revoked.json");
    let mut ids: Vec<String> = if path.exists() {
        let bytes = tokio::fs::read(&path).await?;
        serde_json::from_slice(&bytes).unwrap_or_default()
    } else {
        Vec::new()
    };
    if !ids.iter().any(|x| x == token_id) {
        ids.push(token_id.to_string());
    }
    let json = serde_json::to_string_pretty(&ids)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

async fn write_pem(path: &Path, pem: &str) -> std::io::Result<()> {
    tokio::fs::write(path, pem).await
}

async fn write_pem_secret(path: &Path, pem: &str) -> std::io::Result<()> {
    write_raw_secret(path, pem.as_bytes()).await
}

async fn write_raw_secret(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    tokio::fs::write(path, bytes).await?;
    set_secret_permissions(path).await
}

/// The directory the HTTPS substrate keeps its material under — the
/// parent of `https/`: `EVO_HTTPS_STATE_DIR` when set, else the
/// directory holding the persistence file, else `/var/lib/evo`.
///
/// The steward boots from it and `auth rotate-token-key` rewrites it,
/// so both resolve it here.
pub fn https_state_root(persistence_path: &Path) -> PathBuf {
    match std::env::var_os("EVO_HTTPS_STATE_DIR") {
        Some(v) => PathBuf::from(v),
        None => persistence_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/evo")),
    }
}

/// What [`rotate_token_signing_key`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenKeyRotation {
    /// The `https/` directory the new key was written to.
    pub https_dir: PathBuf,
    /// Credential records removed from the operator inventory.
    pub records_cleared: usize,
    /// Whether a revocation list was present and removed.
    pub revocations_cleared: bool,
    /// Whether a first-boot bootstrap token was present and removed.
    pub bootstrap_token_removed: bool,
}

/// Replace the bearer-token signing key under `https_dir`, offline.
///
/// Every bearer is signed by this key, so once the steward next starts
/// every bearer issued before the rotation is refused: paired browsers
/// pair again and operator credentials are minted anew. A kiosk panel
/// mints a new session within about 15 seconds of that start. What
/// described the old bearers goes with them: the operator credential
/// records, the revocation list,
/// and the first-boot `bootstrap.token` (the next start issues a new
/// one, as on a first boot).
///
/// Refuses while the steward answers on `steward_socket`, because a
/// running steward keeps the old key in memory and would write records
/// signed by it. Refuses when `https_dir` holds no `tokens.key`, so a
/// wrong state directory is never seeded with a key nothing reads.
///
/// The new key is written beside the old one at mode `0600`, owned as
/// the old one was, synced, and renamed over it.
pub fn rotate_token_signing_key(
    https_dir: &Path,
    steward_socket: &Path,
) -> anyhow::Result<TokenKeyRotation> {
    #[cfg(unix)]
    if std::os::unix::net::UnixStream::connect(steward_socket).is_ok() {
        anyhow::bail!(
            "the steward is running: its socket {} answers. Stop the \
             steward service, then rotate.",
            steward_socket.display()
        );
    }
    replace_token_signing_key(https_dir)
}

/// The file `reset_credentials_to_open` leaves under `https/`: the
/// next boot replaces the bearer-token signing key before it loads one.
pub(crate) const TOKEN_KEY_RETIRED: &str = "tokens.key.retire";

/// Retire the bearer-token signing key under `https_dir`: leave
/// [`TOKEN_KEY_RETIRED`] there, synced, so the next boot replaces the
/// key before it loads one and every bearer issued before then is
/// refused. The running steward keeps the key it loaded until it stops.
pub(crate) fn retire_token_signing_key(
    https_dir: &Path,
) -> std::io::Result<()> {
    std::fs::File::create(https_dir.join(TOKEN_KEY_RETIRED))?.sync_all()?;
    std::fs::File::open(https_dir)?.sync_all()
}

/// The file `reset_credentials_to_open` leaves under `https/`: every
/// start runs the `Open` auth tier, whatever the configuration names,
/// until [`restore_configured_tier`] removes it.
pub(crate) const OPEN_TIER_AFTER_RESET: &str = "tier.open";

/// Leave [`OPEN_TIER_AFTER_RESET`] under `https_dir`, synced.
fn leave_open_tier(https_dir: &Path) -> std::io::Result<()> {
    std::fs::File::create(https_dir.join(OPEN_TIER_AFTER_RESET))?.sync_all()?;
    std::fs::File::open(https_dir)?.sync_all()
}

/// Mark a credentials reset under `https_dir`: retire the bearer-token
/// signing key ([`retire_token_signing_key`]) and leave the `Open`
/// tier ([`OPEN_TIER_AFTER_RESET`]). Both or neither: when the tier
/// cannot be left, a retirement this call made is taken back, so a
/// refused reset leaves the device as it was.
pub(crate) fn mark_credentials_reset(https_dir: &Path) -> std::io::Result<()> {
    let retire = https_dir.join(TOKEN_KEY_RETIRED);
    let retired_before = retire.try_exists()?;
    retire_token_signing_key(https_dir)?;
    let Err(e) = leave_open_tier(https_dir) else {
        return Ok(());
    };
    if !retired_before {
        let undone = std::fs::remove_file(&retire)
            .and_then(|()| std::fs::File::open(https_dir)?.sync_all());
        if let Err(undo) = undone {
            return Err(std::io::Error::new(
                e.kind(),
                format!(
                    "{e}; taking back the key retirement failed too \
                     ({undo}), so the next start replaces the key"
                ),
            ));
        }
    }
    Err(e)
}

/// Remove the `Open` tier a credentials reset left under `https_dir`,
/// offline, so the next start runs the tier the configuration names
/// (`EVO_AUTH_TIER`, `Open` when unset). `false` when no reset left
/// one.
///
/// Refuses while the steward answers on `steward_socket`: a running
/// steward holds the tier it runs, and would keep running `Open`.
pub fn restore_configured_tier(
    https_dir: &Path,
    steward_socket: &Path,
) -> anyhow::Result<bool> {
    #[cfg(unix)]
    if std::os::unix::net::UnixStream::connect(steward_socket).is_ok() {
        anyhow::bail!(
            "the steward is running: its socket {} answers. Stop the \
             steward service, then restore the tier.",
            steward_socket.display()
        );
    }
    let marker = https_dir.join(OPEN_TIER_AFTER_RESET);
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "removing {} failed: {e}",
                marker.display()
            ))
        }
    }
    std::fs::File::open(https_dir)?.sync_all()?;
    Ok(true)
}

/// The steward's auth tier: the one its configuration names, or `Open`
/// while a credentials reset holds it there.
///
/// Every admission path holds this one provider, so a reset opens the
/// running steward at once; each start reads
/// [`OPEN_TIER_AFTER_RESET`] again.
#[derive(Debug)]
pub(crate) struct StewardAuthTier {
    configured: Arc<dyn AuthTierProvider>,
    open_after_reset: AtomicBool,
}

impl StewardAuthTier {
    /// `configured`, or `Open` when `open_after_reset`.
    pub(crate) fn new(
        configured: Arc<dyn AuthTierProvider>,
        open_after_reset: bool,
    ) -> Self {
        Self {
            configured,
            open_after_reset: AtomicBool::new(open_after_reset),
        }
    }

    /// The tier the configuration names.
    pub(crate) fn configured(&self) -> AuthTier {
        self.configured.current()
    }

    /// Whether a credentials reset holds the tier at `Open`.
    pub(crate) fn open_after_reset(&self) -> bool {
        self.open_after_reset.load(Ordering::Acquire)
    }

    /// Hold the tier at `Open`: a credentials reset has run.
    pub(crate) fn open(&self) {
        self.open_after_reset.store(true, Ordering::Release);
    }
}

impl AuthTierProvider for StewardAuthTier {
    fn current(&self) -> AuthTier {
        if self.open_after_reset() {
            AuthTier::Open
        } else {
            self.configured()
        }
    }
}

/// Carry out a retirement [`retire_token_signing_key`] left under
/// `https_dir`, before the boot loads a key: replace the key, as
/// [`rotate_token_signing_key`] does, then remove the word. `None` when
/// no key was retired.
///
/// When the retired key is already gone, the boot generates a fresh
/// one; what described the old bearers is cleared all the same. Any
/// failure is returned before the word is removed, so the retired key
/// is never loaded and the next boot tries again.
fn replace_retired_token_key(
    https_dir: &Path,
) -> anyhow::Result<Option<TokenKeyRotation>> {
    let marker = https_dir.join(TOKEN_KEY_RETIRED);
    let replaced = (|| -> anyhow::Result<Option<TokenKeyRotation>> {
        if !marker.try_exists()? {
            return Ok(None);
        }
        let rotation = if https_dir.join("tokens.key").try_exists()? {
            replace_token_signing_key(https_dir)?
        } else {
            clear_bearer_records(https_dir)?
        };
        std::fs::remove_file(&marker)?;
        std::fs::File::open(https_dir)?.sync_all()?;
        Ok(Some(rotation))
    })();
    replaced.map_err(|e| {
        anyhow::anyhow!(
            "the credentials reset retired the bearer-token signing key \
             under {}, and replacing it failed: {e}. The retired key is \
             not loaded; the HTTPS listener stays down until a start \
             replaces it.",
            https_dir.display()
        )
    })
}

/// Replace the bearer-token signing key under `https_dir` and clear
/// what described the bearers it signed. Nothing has loaded the key:
/// the steward is stopped, or booting and not yet at its key.
fn replace_token_signing_key(
    https_dir: &Path,
) -> anyhow::Result<TokenKeyRotation> {
    let key_path = https_dir.join("tokens.key");
    let old = std::fs::metadata(&key_path).map_err(|e| {
        anyhow::anyhow!(
            "no bearer-token signing key at {} ({e}); point --state-dir at \
             the directory the steward boots from",
            key_path.display()
        )
    })?;

    let key = BearerTokenIssuer::generate_signing_key();
    let tmp_path = https_dir.join("tokens.key.rotating");
    write_secret_like(&tmp_path, &key.to_bytes(), &old)?;
    std::fs::rename(&tmp_path, &key_path)?;
    std::fs::File::open(https_dir)?.sync_all()?;

    clear_bearer_records(https_dir)
}

/// Clear what described the bearers a retired key signed: the
/// operator credential records, the revocation list and the first-boot
/// `bootstrap.token` (the next start issues a new one).
fn clear_bearer_records(https_dir: &Path) -> anyhow::Result<TokenKeyRotation> {
    let credentials_dir = https_dir.join("credentials");
    let records_cleared = if credentials_dir.is_dir() {
        CredentialStore::new(credentials_dir)
            .and_then(|store| store.purge())
            .map_err(|e| anyhow::anyhow!("clearing credential records: {e}"))?
    } else {
        0
    };
    let revocations_cleared =
        remove_if_present(&https_dir.join("revoked.json"))?;
    let bootstrap_token_removed =
        remove_if_present(&https_dir.join("bootstrap.token"))?;

    Ok(TokenKeyRotation {
        https_dir: https_dir.to_path_buf(),
        records_cleared,
        revocations_cleared,
        bootstrap_token_removed,
    })
}

/// Write `bytes` to a fresh `path` at mode `0600`, owned as `like` is,
/// and sync it.
fn write_secret_like(
    path: &Path,
    bytes: &[u8],
    like: &std::fs::Metadata,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // A rotation that died before its rename. Its bytes were
            // never live; start again.
            std::fs::remove_file(path)?;
            options.open(path)?
        }
        Err(e) => return Err(e),
    };
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::os::unix::fs::chown(path, Some(like.uid()), Some(like.gid()))?;
    }
    #[cfg(not(unix))]
    let _ = like;
    file.sync_all()
}

fn remove_if_present(path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
async fn set_secret_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(path).await?.permissions();
    perms.set_mode(0o600);
    tokio::fs::set_permissions(path, perms).await
}

#[cfg(not(unix))]
async fn set_secret_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Operator-tier capability set for first-boot bootstrap tokens and
/// SSH-required `mint_bearer_token` wire-op issuances. Both surfaces
/// produce equivalent operator credentials; factoring the set here
/// keeps the two call sites in lockstep when capability scopes
/// evolve.
///
/// Three layers:
///
/// - **Step-up scopes** (`step_up:*_admin` + `write:auth`): the
///   holder may dispatch privileged mutations through the step-up
///   elevation gateway. `write:auth` reaches `step_up_auth_verify`
///   to mint short-lived freshly-authenticated step-up sessions
///   backed by the configured `AuthService`.
/// - **Read scopes** (`read:*`): cover the entire wire schema's
///   `describe` / `list` / `get` surface so the schema-first UI
///   shell can paint every shelf without a separate elevation. A
///   first-time operator opening the device's UI must see device
///   state immediately; restricting any of these defeats the
///   bootstrap UX.
/// - **Non-step-up write scopes**: operator-driven mutations that
///   don't require an active step-up session (UI affordances,
///   plugin requests, multi-room group composition, plan
///   scheduling, prompt-responder role).
///
/// The `user_interaction_responder` scope belongs to the third
/// group: the responder is an operator role that answers plugin-
/// initiated prompts (device pairing, credential entry, source-
/// disposition modals). Without the scope in this set, no minted
/// operator token can grant the responder capability on an HTTPS-
/// arriving connection — the negotiate handler's bearer branch has
/// nothing to observe in `granted_capabilities`. See the
/// `operator_bootstrap_set_carries_responder` test below.
pub fn operator_bootstrap_capability_set() -> CapabilitySet {
    CapabilitySet::new(vec![
        Capability::step_up("plugins_admin"),
        Capability::step_up("audio_admin"),
        Capability::step_up("watches_admin"),
        Capability::step_up("appointments_admin"),
        Capability::step_up("plans_admin"),
        Capability::step_up("updates_admin"),
        Capability::step_up("subjects_admin"),
        Capability::step_up("system_admin"),
        Capability::step_up("reconciliation_admin"),
        Capability::write("auth"),
        Capability::read("plugins"),
        Capability::read("subjects"),
        Capability::read("audio"),
        Capability::read("multiroom"),
        Capability::read("plans"),
        Capability::read("updates"),
        Capability::read("system"),
        Capability::read("auth"),
        Capability::read("discovery"),
        Capability::read("ui"),
        Capability::read("observatory"),
        Capability::read("witness"),
        Capability::write("ui"),
        Capability::write("plugins"),
        Capability::write("multiroom"),
        Capability::write("plans"),
        Capability::write("request"),
        Capability::write("user_interaction_responder"),
        // The pairing ops. A bearer carries the scope so a paired
        // browser can pair again on its own socket; every no-bearer
        // set carries it too (see `PAIRING_SCOPE`).
        Capability::write(PAIRING_SCOPE),
        // Credential-vault operator surface — the UI writes plugin
        // credentials (third-party API keys / service passwords /
        // OAuth tokens) via `credential_put` / `credential_delete`
        // and reads the plugin's inventory via `credential_list_keys`.
        // Read + write scopes stay operator-bootstrap; there is no
        // `credential_get` op by design (values leave the vault only
        // via plugin-side LoadContext handles).
        Capability::write("credentials"),
        Capability::read("credentials"),
        // Online-provider per-source enable + priority store — the
        // UI's Settings → Metadata → Sources surface lists via
        // `online_providers_list` and mutates via
        // `online_providers_set_enabled` /
        // `online_providers_set_priority`. Both scopes are operator-
        // bootstrap so the pairing surface (post-`pair_complete`)
        // can render Settings + apply toggles without a separate
        // step-up ceremony. Mutations publish on the framework's
        // change bus so plugin reactors re-resolve their local
        // cascade view live.
        Capability::write("online_providers"),
        Capability::read("online_providers"),
        // Settings → Network + Sources → shares / SMB-server /
        // USB. Pair and kiosk mint from this set. The rank is
        // StepUp: the holder may elevate on the scope. It is not
        // a sitting — a verb declared step-up still needs a
        // step-up session on the call. Must NOT appear on
        // LAN-trust (see LAN_TRUST_PRIVILEGED_EXCLUDED_SCOPES) —
        // an anonymous LAN peer must not change Wi-Fi or mounts.
        Capability::step_up("network_admin"),
    ])
}

/// Return the operator bootstrap capability set unioned with
/// every scope declared by an admitted plugin's
/// `[capabilities.respondent.verb_capabilities]` or
/// `[capabilities.warden.verb_capabilities]` manifest map.
///
/// Rationale: the hardcoded [`operator_bootstrap_capability_set`]
/// enumerates only the framework-defined operator scopes
/// (`plugins_admin`, `audio_admin`, …). Plugins declare their
/// own step-up scopes in their manifests (e.g.
/// `org.evoframework.network.smb-server` gates its mutating
/// verbs on `step_up:network_admin`). Without merging, no mint
/// path can produce a token that carries a plugin-declared
/// scope — `mint-bearer-token --scope network_admin` would
/// refuse with `empty_capability_intersection`, and every
/// plugin-declared step-up verb becomes unreachable from the
/// wire. The merge closes that gap systemically for every
/// plugin's declared scopes, not one-by-one.
///
/// A plugin-added scope enters at the highest rank any declaration
/// gives it (`read` < `write` < `step_up`), so a scope declared
/// `write` on one verb and `read` on another enters as `write` and
/// a bearer minted with it passes both. A scope the framework set
/// already holds keeps the framework's rank: a plugin declaring
/// `plugins_admin`, or `write` on a scope the framework holds at
/// `read`, changes nothing.
pub fn merged_operator_bootstrap_capability_set(
    router: &crate::router::PluginRouter,
) -> CapabilitySet {
    use evo_plugin_sdk::manifest::VerbCapability;
    let mut plugin_scopes: Vec<VerbCapability> = Vec::new();
    for entry in router.entries_in_order() {
        let policy = entry.load_policy();
        plugin_scopes
            .extend(policy.respondent_verb_capabilities.values().cloned());
        plugin_scopes.extend(policy.warden_verb_capabilities.values().cloned());
    }
    merge_bootstrap_with_plugin_scopes(
        &operator_bootstrap_capability_set(),
        plugin_scopes.iter(),
    )
}

/// Pure merge logic: union the base capability set with every
/// scope observed in the supplied iterator, each at the highest rank
/// declared for it, in the order the scopes first appear.
/// `VerbCapability::None` entries name no scope and are ignored.
/// Base entries win on scope-name collision (a plugin re-declaring
/// `plugins_admin` produces no new entry and no new rank).
///
/// Isolated from [`merged_operator_bootstrap_capability_set`] so
/// the merge invariants can be unit-tested against a fixture set
/// without constructing a real `PluginRouter`.
pub(crate) fn merge_bootstrap_with_plugin_scopes<'a>(
    base: &CapabilitySet,
    verb_capabilities: impl IntoIterator<
        Item = &'a evo_plugin_sdk::manifest::VerbCapability,
    >,
) -> CapabilitySet {
    use evo_plugin_sdk::manifest::VerbCapability;
    use std::collections::HashSet;
    let framework: HashSet<&str> =
        base.capabilities().iter().map(|c| c.scope()).collect();
    // Each scope the plugins add, at the highest rank declared for it:
    // 1 read, 2 write, 3 step-up.
    let mut added: Vec<(String, u8)> = Vec::new();
    for cap in verb_capabilities {
        let (scope, rank) = match cap {
            VerbCapability::None => continue,
            VerbCapability::Read { scope } => (scope, 1),
            VerbCapability::Write { scope } => (scope, 2),
            VerbCapability::StepUp { scope } => (scope, 3),
        };
        if framework.contains(scope.as_str()) {
            continue;
        }
        match added.iter_mut().find(|(s, _)| s == scope) {
            Some((_, held)) => *held = (*held).max(rank),
            None => added.push((scope.clone(), rank)),
        }
    }
    let mut merged: Vec<Capability> = base.capabilities().to_vec();
    merged.extend(added.into_iter().map(|(scope, rank)| match rank {
        1 => Capability::read(scope),
        2 => Capability::write(scope),
        _ => Capability::step_up(scope),
    }));
    CapabilitySet::new(merged)
}

/// Names of every capability scope in
/// [`operator_bootstrap_capability_set`] that carries
/// single-holder role semantics (at-most-one connection may
/// exercise the capability at a time; a mutex on the ledger
/// serialises claims). These scopes must NEVER appear in the
/// LAN-trust default capability set — LAN-trust is broad-
/// audience (any browser on the LAN is admitted without a
/// bearer under the Open / Secure-LAN tiers) and a single-
/// holder role composes catastrophically with a broad-
/// audience admission path: any device on the LAN would be
/// able to claim the role from the operator's actual UI
/// session. Legitimate LAN-side responders reach the role
/// through the bearer path (an operator-issued scoped mint
/// via `evo-plugin-tool admin auth mint-bearer-token --scope
/// user_interaction_responder`).
const SINGLE_HOLDER_ROLE_SCOPES: &[&str] = &["user_interaction_responder"];

/// The scope the pairing ops (`pair_begin`, `pair_complete`,
/// `pair_authenticate`) declare, at write rank.
///
/// Pairing is how a caller with no bearer earns one, so every
/// no-bearer set carries `write:pair` and no configured bound removes
/// it (see [`HttpsBootConfig::lan_trust_capabilities`]). Each op then
/// authenticates itself, and the steward serves the three to a LAN
/// origin only.
pub const PAIRING_SCOPE: &str = "pair";

/// Privileged operator scopes that live on the bootstrap set but
/// must never be granted to an anonymous LAN-trust peer.
/// `network_admin` mutates Wi-Fi / AP / mounts.
/// `system_admin` mutates kiosk writers (`set_cursor`, `set_osk`).
/// `audio_admin` selects the DAC, sets DSP controls and overlays,
/// and writes the audio policies and topology.
/// Field 2026-09-06: Open HTTPS ran `set_cursor` without a bearer
/// because the LAN-trust arm never consulted this set.
const LAN_TRUST_PRIVILEGED_EXCLUDED_SCOPES: &[&str] =
    &["network_admin", "system_admin", "audio_admin"];

/// LAN-trust default capability set — the subset of
/// [`operator_bootstrap_capability_set`] that is safe to grant
/// to an anonymous LAN peer under Open / Secure-LAN admission.
///
/// Concretely: every scope from the operator bootstrap set,
/// minus `SINGLE_HOLDER_ROLE_SCOPES` and
/// `LAN_TRUST_PRIVILEGED_EXCLUDED_SCOPES`.
/// Read + non-step-up-write scopes compose safely with broad-
/// audience LAN admission (multiple LAN peers can hold them
/// concurrently without disrupting each other); single-holder
/// roles and privileged writers (network, kiosk, audio hardware)
/// do not, and are refused here regardless of distribution config.
///
/// Vendor distributions that want to narrow the LAN-trust set
/// further (e.g. drop `plugins:write` on a kiosk) can override
/// via [`HttpsBootConfig::lan_trust_capabilities`]; those
/// overrides land on top of this floor, they cannot widen
/// past it.
pub fn lan_trust_capability_set() -> CapabilitySet {
    let full = operator_bootstrap_capability_set();
    let filtered: Vec<Capability> = full
        .capabilities()
        .iter()
        .filter(|c| {
            !SINGLE_HOLDER_ROLE_SCOPES.contains(&c.scope())
                && !LAN_TRUST_PRIVILEGED_EXCLUDED_SCOPES.contains(&c.scope())
        })
        .cloned()
        .collect();
    CapabilitySet::new(filtered)
}

/// LAN-origin privileged capability set — the operator bootstrap
/// set minus `SINGLE_HOLDER_ROLE_SCOPES` only.
///
/// This is the set stamped on a no-bearer principal whose effective
/// origin is LAN *when the steward carries a household-protection
/// runtime*. `network_admin`, `system_admin` and `audio_admin`
/// return to it, which is what lets an ordinary household change
/// Wi-Fi, rotate the panel or pick the DAC without a pairing
/// ceremony.
///
/// Two things keep that safe, and neither may be dropped:
///
/// 1. **Origin.** WAN never receives this set. `AuthTier::Open`
///    admits WAN with [`lan_trust_capability_set`] — the playback
///    floor — and only that.
/// 2. **A gate that exists.** The set is offered only when a
///    household-protection runtime is attached, so a privileged
///    scope is never stamped with nothing to police it at dispatch.
///
/// `SINGLE_HOLDER_ROLE_SCOPES` stays excluded here for the reason
/// given at its declaration: a single-holder role composes
/// catastrophically with broad-audience admission, and no household
/// policy changes that.
pub fn lan_privileged_capability_set() -> CapabilitySet {
    let full = operator_bootstrap_capability_set();
    let filtered: Vec<Capability> = full
        .capabilities()
        .iter()
        .filter(|c| !SINGLE_HOLDER_ROLE_SCOPES.contains(&c.scope()))
        .cloned()
        .collect();
    CapabilitySet::new(filtered)
}

/// The sets the no-bearer arms stamp: `(floor, lan_privileged)`.
///
/// The floor serves WAN under `AuthTier::Open` and the LAN when no
/// privileged set is offered; `privileged` is the LAN set when one is.
/// A `bound` narrows both alike and widens neither. Without one, the
/// floor is [`lan_trust_capability_set`] and `privileged` passes
/// through. Either way both carry `write:pair` ([`PAIRING_SCOPE`]).
fn no_bearer_capability_sets(
    bound: Option<&CapabilitySet>,
    privileged: Option<&CapabilitySet>,
) -> (CapabilitySet, Option<CapabilitySet>) {
    let floor = lan_trust_capability_set();
    let (floor, privileged) = match bound {
        Some(bound) => (
            narrow_capability_set(&floor, bound),
            privileged.map(|p| narrow_capability_set(p, bound)),
        ),
        None => (floor, privileged.cloned()),
    };
    (with_pairing(floor), privileged.map(with_pairing))
}

/// `set` with `write:pair` in it: the one capability no no-bearer set
/// goes without, whatever bound narrowed it.
fn with_pairing(set: CapabilitySet) -> CapabilitySet {
    let pairing =
        evo_projection_core::CapabilityRequirement::write(PAIRING_SCOPE);
    if set.satisfies(&pairing) {
        return set;
    }
    let mut caps = set.capabilities().to_vec();
    caps.push(Capability::write(PAIRING_SCOPE));
    CapabilitySet::new(caps)
}

/// Narrow `set` by `bound`: keep each scope both name, at the lower
/// of the two ranks (read < write < step-up).
///
/// The result never holds a scope `set` lacks, nor a rank above the
/// one `set` gives it, so a bound can only take away. An empty bound
/// yields an empty set. Order follows `set`; one capability per
/// scope.
pub fn narrow_capability_set(
    set: &CapabilitySet,
    bound: &CapabilitySet,
) -> CapabilitySet {
    fn rank(c: &Capability) -> u8 {
        match c {
            Capability::Read { .. } => 0,
            Capability::Write { .. } => 1,
            Capability::StepUp { .. } => 2,
        }
    }
    fn highest(set: &CapabilitySet, scope: &str) -> Option<u8> {
        set.capabilities()
            .iter()
            .filter(|c| c.scope() == scope)
            .map(rank)
            .max()
    }
    let mut out: Vec<Capability> = Vec::new();
    for cap in set.capabilities() {
        let scope = cap.scope();
        if out.iter().any(|c| c.scope() == scope) {
            continue;
        }
        let (Some(held), Some(allowed)) =
            (highest(set, scope), highest(bound, scope))
        else {
            continue;
        };
        out.push(match held.min(allowed) {
            0 => Capability::read(scope),
            1 => Capability::write(scope),
            _ => Capability::step_up(scope),
        });
    }
    CapabilitySet::new(out)
}

/// Shorter convenience: the operator-facing log line summarising what
/// was minted at first boot. Returns `None` when this is a subsequent
/// boot.
pub fn first_boot_advisory(handles: &HttpsBootHandles) -> Option<String> {
    handles.bootstrap_token_b64.as_ref().map(|tok| {
        format!(
            "HTTPS first boot: bootstrap operator token persisted under \
             state_dir/https/bootstrap.token. \
             Token length: {len} chars. \
             Listener bound on {addr}.",
            len = tok.len(),
            addr = handles.server.local_addr,
        )
    })
}

/// Marker used in tests to ensure boot_https takes effect even when
/// the caller drops its tokio runtime quickly — gives the rotator a
/// chance to schedule.
#[doc(hidden)]
pub fn _rotation_warmup() -> Duration {
    Duration::from_millis(20)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::CapabilityRequirement;

    #[test]
    fn a_listener_bound_reads_a_whole_number_greater_than_zero() {
        let name = "EVO_HTTPS_MAX_INCOMPLETE_CONNECTIONS";
        assert_eq!(parse_positive(name, None).unwrap(), None);
        assert_eq!(
            parse_positive(name, Some(OsStr::new("256")))
                .unwrap()
                .map(NonZeroU64::get),
            Some(256)
        );
        for refused in ["0", "-1", "", " 8", "8s", "1.5", "lots"] {
            let error = parse_positive(name, Some(OsStr::new(refused)))
                .expect_err(refused)
                .to_string();
            assert!(error.contains(name), "{refused:?}: {error}");
        }
    }

    #[test]
    fn the_listener_bounds_default_to_the_listener_defaults() {
        let config = HttpsBootConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            PathBuf::from("/nonexistent"),
        );
        assert_eq!(
            config.handshake_timeout,
            evo_runtime_http::config::DEFAULT_HANDSHAKE_TIMEOUT
        );
        assert_eq!(
            config.max_incomplete_connections,
            evo_runtime_http::config::DEFAULT_MAX_INCOMPLETE_CONNECTIONS
        );
    }

    #[test]
    fn operator_bootstrap_set_carries_responder() {
        let set = operator_bootstrap_capability_set();
        let scopes: Vec<&str> =
            set.capabilities().iter().map(|c| c.scope()).collect();
        assert!(
            scopes.contains(&"user_interaction_responder"),
            "operator bootstrap capability set must carry \
             `user_interaction_responder` — otherwise no minted \
             operator token can grant the responder role on an \
             HTTPS-arriving connection, and the negotiate handler's \
             bearer branch is unreachable. Actual scopes: {scopes:?}"
        );
    }

    #[test]
    fn operator_bootstrap_set_responder_is_write_rank() {
        let set = operator_bootstrap_capability_set();
        let responder = set
            .capabilities()
            .iter()
            .find(|c| c.scope() == "user_interaction_responder")
            .expect("responder must be present");
        assert!(
            matches!(responder, evo_auth_bearer::Capability::Write { .. }),
            "responder submits a response payload (state mutation \
             on the prompt subject) — Write is the honest rank; \
             Read is technically sufficient for the bearer branch \
             but understates what the role does. Actual: {responder:?}"
        );
    }

    #[test]
    fn operator_bootstrap_set_carries_network_admin_step_up() {
        let set = operator_bootstrap_capability_set();
        let network_admin = set
            .capabilities()
            .iter()
            .find(|c| c.scope() == "network_admin")
            .expect(
                "operator bootstrap must carry network_admin so pair/kiosk \
                 mint admits Settings → Network and share mutations without \
                 depending on plugin-merge at mint time",
            );
        assert!(
            matches!(network_admin, Capability::StepUp { .. }),
            "network_admin must land as StepUp so the holder may elevate \
             on it. Actual: {network_admin:?}"
        );
    }

    #[test]
    fn playback_floor_excludes_network_admin() {
        // The floor is what WAN gets, and what LAN gets when no
        // household-protection runtime exists to police a wider
        // stamp. network_admin mutates Wi-Fi, AP, flight and share
        // mounts, so it is never on the floor.
        let set = lan_trust_capability_set();
        let scopes: Vec<&str> =
            set.capabilities().iter().map(|c| c.scope()).collect();
        assert!(
            !scopes.contains(&"network_admin"),
            "playback floor must NOT carry network_admin. \
             Actual scopes: {scopes:?}"
        );
    }

    #[test]
    fn lan_privileged_set_returns_network_and_system_admin() {
        // The LAN arm, offered only alongside a runtime. This is
        // what lets a household change Wi-Fi or rotate the panel
        // without a pairing ceremony; the dispatch gate is what
        // refuses it when the policy protects the group.
        let set = lan_privileged_capability_set();
        let scopes: Vec<&str> =
            set.capabilities().iter().map(|c| c.scope()).collect();
        assert!(scopes.contains(&"network_admin"));
        assert!(scopes.contains(&"system_admin"));
        assert!(set.satisfies(&CapabilityRequirement::write("system_admin")));
    }

    #[test]
    fn lan_privileged_set_still_excludes_the_single_holder_role() {
        // No household policy changes this: a single-holder role
        // composes catastrophically with broad-audience admission,
        // whatever the operator chose on the ladder.
        let set = lan_privileged_capability_set();
        let scopes: Vec<&str> =
            set.capabilities().iter().map(|c| c.scope()).collect();
        assert!(
            !scopes.contains(&"user_interaction_responder"),
            "privileged LAN set must NOT carry the single-holder \
             role. Actual scopes: {scopes:?}"
        );
    }

    #[test]
    fn privileged_set_is_the_floor_plus_exactly_the_privileged_scopes() {
        // Pins the delta so a future widening has to be deliberate.
        use std::collections::BTreeSet;
        let floor_set = lan_trust_capability_set();
        let privileged_set = lan_privileged_capability_set();
        let floor: BTreeSet<String> = floor_set
            .capabilities()
            .iter()
            .map(|c| c.scope().to_owned())
            .collect();
        let privileged: BTreeSet<String> = privileged_set
            .capabilities()
            .iter()
            .map(|c| c.scope().to_owned())
            .collect();
        let added: BTreeSet<String> =
            privileged.difference(&floor).cloned().collect();
        let expected: BTreeSet<String> = [
            "network_admin".to_owned(),
            "system_admin".to_owned(),
            "audio_admin".to_owned(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            added, expected,
            "the LAN privileged arm must add exactly network_admin, \
             system_admin and audio_admin over the playback floor"
        );
    }

    #[test]
    fn playback_floor_excludes_audio_admin() {
        // The floor is what a public origin gets under Open.
        // audio_admin selects the DAC and sets DSP controls; its
        // writers declare write rank, which a step-up rank on the
        // floor would satisfy. A household's LAN reaches them through
        // the privileged arm, a paired browser through its bearer.
        let set = lan_trust_capability_set();
        let scopes: Vec<&str> =
            set.capabilities().iter().map(|c| c.scope()).collect();
        assert!(
            !scopes.contains(&"audio_admin"),
            "playback floor must NOT carry audio_admin. \
             Actual scopes: {scopes:?}"
        );
        assert!(
            !set.satisfies(&CapabilityRequirement::write("audio_admin")),
            "the playback floor must not satisfy write:audio_admin"
        );
        assert!(
            lan_privileged_capability_set()
                .satisfies(&CapabilityRequirement::write("audio_admin")),
            "the LAN privileged set keeps write:audio_admin"
        );
        assert!(
            operator_bootstrap_capability_set()
                .satisfies(&CapabilityRequirement::write("audio_admin")),
            "pair and kiosk mint from the bootstrap set, which keeps \
             write:audio_admin"
        );
    }

    #[test]
    fn playback_floor_excludes_system_admin() {
        // Field 2026-09-06: Open HTTPS ran set_cursor
        // (write:system_admin) because the LAN-trust arm skipped the
        // set check and the set still carried system_admin. The
        // floor still drops the scope; what changed with household
        // protection is that a LAN principal may now be stamped from
        // the privileged arm instead — and only when a gate exists.
        // WAN still lands here.
        let set = lan_trust_capability_set();
        let scopes: Vec<&str> =
            set.capabilities().iter().map(|c| c.scope()).collect();
        assert!(
            !scopes.contains(&"system_admin"),
            "LAN-trust capability set must NOT carry system_admin. \
             Actual scopes: {scopes:?}"
        );
        assert!(
            !set.satisfies(&CapabilityRequirement::write("system_admin")),
            "LAN-trust must not satisfy write:system_admin"
        );
    }

    #[test]
    fn lan_trust_set_excludes_user_interaction_responder() {
        // Security-critical: LAN-trust admission grants an
        // anonymous LAN peer this capability set without any
        // bearer. Including a single-holder role scope here
        // would let any LAN device race the operator's UI for
        // the responder claim.
        let set = lan_trust_capability_set();
        let scopes: Vec<&str> =
            set.capabilities().iter().map(|c| c.scope()).collect();
        assert!(
            !scopes.contains(&"user_interaction_responder"),
            "LAN-trust capability set must NOT carry \
             `user_interaction_responder` — LAN-trust is \
             broad-audience admission and the responder is a \
             single-holder role. Actual scopes: {scopes:?}"
        );
    }

    #[test]
    fn merge_admits_plugin_declared_step_up_scope() {
        // Regression fixture: a plugin declares `step_up:network_admin`
        // in its manifest but the framework's hardcoded bootstrap
        // set doesn't enumerate it. Pre-merge, no mint path could
        // produce a token carrying that scope. Post-merge, the
        // scope is present with StepUp rank so `--scope
        // network_admin` narrowing intersects to a valid token.
        use evo_plugin_sdk::manifest::VerbCapability;
        let base = operator_bootstrap_capability_set();
        let plugin_declared = [VerbCapability::StepUp {
            scope: "network_admin".to_string(),
        }];
        let merged =
            merge_bootstrap_with_plugin_scopes(&base, plugin_declared.iter());
        let scopes: Vec<&str> =
            merged.capabilities().iter().map(|c| c.scope()).collect();
        assert!(
            scopes.contains(&"network_admin"),
            "merged set must carry plugin-declared network_admin; \
             got {scopes:?}"
        );
        let network_admin = merged
            .capabilities()
            .iter()
            .find(|c| c.scope() == "network_admin")
            .unwrap();
        assert!(
            matches!(network_admin, Capability::StepUp { .. }),
            "plugin-declared scope must land as StepUp rank"
        );
    }

    #[test]
    fn merge_admits_plugin_declared_read_and_write_scopes() {
        // A plugin's own `read` and `write` scopes enter the set a
        // bearer is minted from, each at its declared rank. `None`
        // names no scope and adds nothing.
        use evo_plugin_sdk::manifest::VerbCapability;
        let base = operator_bootstrap_capability_set();
        let plugin_declared = [
            VerbCapability::None,
            VerbCapability::Read {
                scope: "voucher_list".to_string(),
            },
            VerbCapability::Write {
                scope: "voucher_issue".to_string(),
            },
        ];
        let merged =
            merge_bootstrap_with_plugin_scopes(&base, plugin_declared.iter());
        assert_eq!(merged.len(), base.len() + 2);
        let added: Vec<&Capability> =
            merged.capabilities()[base.len()..].iter().collect();
        assert_eq!(
            added,
            vec![
                &Capability::read("voucher_list"),
                &Capability::write("voucher_issue"),
            ]
        );
    }

    #[test]
    fn merge_takes_the_highest_rank_declared_for_a_scope() {
        // One scope declared at several ranks, by one plugin or many,
        // in any order, enters once at the highest of them, so a bearer
        // minted with it passes every verb that declares it.
        use evo_plugin_sdk::manifest::VerbCapability;
        let base = operator_bootstrap_capability_set();
        let read = |s: &str| VerbCapability::Read {
            scope: s.to_string(),
        };
        let write = |s: &str| VerbCapability::Write {
            scope: s.to_string(),
        };
        let step_up = |s: &str| VerbCapability::StepUp {
            scope: s.to_string(),
        };
        let plugin_declared = [
            read("voucher_issue"),
            write("voucher_issue"),
            read("voucher_issue"),
            write("desk_admin"),
            step_up("desk_admin"),
            read("desk_admin"),
        ];
        let merged =
            merge_bootstrap_with_plugin_scopes(&base, plugin_declared.iter());
        assert_eq!(
            merged.capabilities()[base.len()..].to_vec(),
            vec![
                Capability::write("voucher_issue"),
                Capability::step_up("desk_admin"),
            ]
        );
    }

    #[test]
    fn merge_never_raises_a_framework_scope() {
        // The framework's set holds `system` and `audio` at read and
        // the admin scopes at step-up. A plugin declaring another rank
        // on them changes nothing: a plugin manifest cannot widen what
        // every pair and kiosk bearer holds on the framework's scopes.
        use evo_plugin_sdk::manifest::VerbCapability;
        let base = operator_bootstrap_capability_set();
        let plugin_declared = [
            VerbCapability::Write {
                scope: "system".to_string(),
            },
            VerbCapability::StepUp {
                scope: "audio".to_string(),
            },
            VerbCapability::Read {
                scope: "network_admin".to_string(),
            },
        ];
        let merged =
            merge_bootstrap_with_plugin_scopes(&base, plugin_declared.iter());
        assert_eq!(merged, base);
    }

    #[test]
    fn merge_deduplicates_scope_collision() {
        // Base already carries `plugins_admin` as StepUp. A
        // plugin re-declaring the same scope must not produce a
        // duplicate entry — base wins on the collision.
        use evo_plugin_sdk::manifest::VerbCapability;
        let base = operator_bootstrap_capability_set();
        let base_len = base.len();
        let plugin_declared = [VerbCapability::StepUp {
            scope: "plugins_admin".to_string(),
        }];
        let merged =
            merge_bootstrap_with_plugin_scopes(&base, plugin_declared.iter());
        assert_eq!(
            merged.len(),
            base_len,
            "same-scope re-declaration must not duplicate"
        );
    }

    #[test]
    fn merge_dedupes_multiple_plugins_declaring_same_scope() {
        // Two different plugins can both declare
        // `step_up:network_admin` (e.g. shares admin + smb-server
        // admin under the same operator umbrella). One entry
        // survives.
        use evo_plugin_sdk::manifest::VerbCapability;
        let base = operator_bootstrap_capability_set();
        let plugin_declared = [
            VerbCapability::StepUp {
                scope: "network_admin".to_string(),
            },
            VerbCapability::StepUp {
                scope: "network_admin".to_string(),
            },
        ];
        let merged =
            merge_bootstrap_with_plugin_scopes(&base, plugin_declared.iter());
        let network_admin_count = merged
            .capabilities()
            .iter()
            .filter(|c| c.scope() == "network_admin")
            .count();
        assert_eq!(
            network_admin_count, 1,
            "multiple plugins declaring the same scope must \
             produce exactly one merged entry"
        );
    }

    #[test]
    fn lan_trust_set_is_operator_bootstrap_minus_excluded_scopes() {
        // Invariant: LAN-trust set is the operator bootstrap set
        // minus single-holder roles and privileged excluded scopes.
        // Guards against a future edit adding a privileged scope
        // to the bootstrap set that inadvertently widens LAN-trust.
        let bootstrap = operator_bootstrap_capability_set();
        let lan_trust = lan_trust_capability_set();
        let excluded_count = SINGLE_HOLDER_ROLE_SCOPES.len()
            + LAN_TRUST_PRIVILEGED_EXCLUDED_SCOPES.len();
        assert_eq!(
            lan_trust.len(),
            bootstrap.len() - excluded_count,
            "LAN-trust set size drifted from bootstrap - excluded"
        );
        let lan_scopes: std::collections::HashSet<&str> =
            lan_trust.capabilities().iter().map(|c| c.scope()).collect();
        for excluded in SINGLE_HOLDER_ROLE_SCOPES
            .iter()
            .chain(LAN_TRUST_PRIVILEGED_EXCLUDED_SCOPES.iter())
        {
            assert!(
                !lan_scopes.contains(excluded),
                "LAN-trust must exclude {excluded}"
            );
        }
    }

    #[test]
    fn narrowing_keeps_the_lower_rank_and_never_widens() {
        let set = CapabilitySet::new(vec![
            Capability::step_up("plugins_admin"),
            Capability::write("auth"),
            Capability::read("auth"),
            Capability::read("audio"),
        ]);
        let bound = CapabilitySet::new(vec![
            // Lower than held: kept at the bound's rank.
            Capability::write("plugins_admin"),
            // Higher than held: kept at the held rank, not raised.
            Capability::step_up("audio"),
            // Not held at all: never added.
            Capability::step_up("network_admin"),
        ]);
        let out = narrow_capability_set(&set, &bound);
        assert_eq!(
            out,
            CapabilitySet::new(vec![
                Capability::write("plugins_admin"),
                Capability::read("audio"),
            ]),
        );
        assert!(
            narrow_capability_set(&set, &CapabilitySet::default()).is_empty()
        );
    }

    /// Regression pin: a steward that sets no bound stamps the
    /// no-bearer sets every steward has had. The home player's LAN
    /// keeps its floor and its privileged set.
    #[test]
    fn the_default_floor_keeps_the_home_player() {
        let privileged = lan_privileged_capability_set();
        let (floor, lan) = no_bearer_capability_sets(None, Some(&privileged));
        assert_eq!(floor, lan_trust_capability_set());
        assert_eq!(lan, Some(privileged));
        let (floor, lan) = no_bearer_capability_sets(None, None);
        assert_eq!(floor, lan_trust_capability_set());
        assert_eq!(lan, None);
    }

    /// A bound bounds every no-bearer arm, the LAN privileged
    /// set included; an empty bound leaves both holding nothing.
    #[test]
    fn a_bound_narrows_every_no_bearer_arm() {
        let privileged = lan_privileged_capability_set();
        let bound = CapabilitySet::new(vec![Capability::read("audio")]);
        let narrowed = CapabilitySet::new(vec![
            Capability::read("audio"),
            Capability::write(PAIRING_SCOPE),
        ]);
        let (floor, lan) =
            no_bearer_capability_sets(Some(&bound), Some(&privileged));
        assert_eq!(floor, narrowed);
        assert_eq!(lan, Some(narrowed));
    }

    /// An empty bound leaves every no-bearer set holding
    /// pairing and nothing else. No bound can take pairing away.
    #[test]
    fn an_empty_floor_holds_nothing_but_pairing() {
        let pairing_only =
            CapabilitySet::new(vec![Capability::write(PAIRING_SCOPE)]);
        let privileged = lan_privileged_capability_set();
        let empty = CapabilitySet::default();
        let (floor, lan) =
            no_bearer_capability_sets(Some(&empty), Some(&privileged));
        assert_eq!(floor, pairing_only);
        assert_eq!(lan, Some(pairing_only.clone()));
        assert!(!floor.satisfies(&CapabilityRequirement::write("auth")));
        assert!(!floor.satisfies(&CapabilityRequirement::read("auth")));
    }

    #[test]
    fn every_bearer_and_every_no_bearer_set_can_pair() {
        let pairing = CapabilityRequirement::write(PAIRING_SCOPE);
        assert!(operator_bootstrap_capability_set().satisfies(&pairing));
        assert!(lan_trust_capability_set().satisfies(&pairing));
        assert!(lan_privileged_capability_set().satisfies(&pairing));
        for schema_op in ["pair_begin", "pair_complete", "pair_authenticate"] {
            let op = crate::projection_schema::canonical_schema()
                .into_iter()
                .find(|o| o.id.as_str() == schema_op)
                .expect("pairing op in the schema");
            assert_eq!(op.capability, pairing, "{schema_op}");
        }
    }

    /// A key the reset retired that is already gone leaves the boot to
    /// generate a fresh one; what described the old bearers is cleared
    /// all the same, and the word is used once.
    #[test]
    fn a_retired_key_already_gone_still_clears_what_described_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let https = dir.path();
        std::fs::write(https.join("revoked.json"), r#"["t1"]"#).expect("list");
        std::fs::write(https.join("bootstrap.token"), "b").expect("token");
        retire_token_signing_key(https).expect("retire");

        let rotation = replace_retired_token_key(https)
            .expect("replaced")
            .expect("a key was retired");
        assert!(rotation.revocations_cleared);
        assert!(rotation.bootstrap_token_removed);
        assert!(!https.join("revoked.json").exists());
        assert!(!https.join("bootstrap.token").exists());
        assert!(!https.join(TOKEN_KEY_RETIRED).exists());
        assert!(!https.join("tokens.key").exists(), "the boot generates it");
        assert_eq!(replace_retired_token_key(https).expect("again"), None);
    }

    /// The steward admits by the configured tier until a reset opens
    /// it; the configured tier is still what the configuration names.
    #[test]
    fn the_tier_is_the_configured_one_until_a_reset_opens_it() {
        let configured: Arc<dyn AuthTierProvider> =
            Arc::new(evo_runtime_http::StaticAuthTier::new(AuthTier::Secure));
        let tier = StewardAuthTier::new(Arc::clone(&configured), false);
        assert_eq!(tier.current(), AuthTier::Secure);
        tier.open();
        assert_eq!(tier.current(), AuthTier::Open);
        assert_eq!(tier.configured(), AuthTier::Secure);
        let reset_earlier = StewardAuthTier::new(configured, true);
        assert_eq!(reset_earlier.current(), AuthTier::Open);
    }

    /// A reset leaves both marks or neither: when the Open tier cannot
    /// be left, the key retirement this reset made is taken back, and
    /// one an earlier reset left stays.
    #[test]
    fn a_reset_leaves_both_marks_or_neither() {
        let dir = tempfile::tempdir().expect("tempdir");
        let https = dir.path();
        mark_credentials_reset(https).expect("marked");
        assert!(https.join(TOKEN_KEY_RETIRED).exists());
        assert!(https.join(OPEN_TIER_AFTER_RESET).exists());

        let dir = tempfile::tempdir().expect("tempdir");
        let https = dir.path();
        // A directory where the file goes cannot be created as one.
        std::fs::create_dir(https.join(OPEN_TIER_AFTER_RESET)).expect("dir");
        mark_credentials_reset(https).expect_err("the tier cannot be left");
        assert!(!https.join(TOKEN_KEY_RETIRED).exists(), "taken back");

        retire_token_signing_key(https).expect("an earlier reset");
        mark_credentials_reset(https).expect_err("the tier cannot be left");
        assert!(https.join(TOKEN_KEY_RETIRED).exists(), "the earlier stays");
    }

    /// Restoring the configured tier removes the Open tier a reset
    /// left, once; with none left it changes nothing.
    #[test]
    fn restoring_the_tier_removes_the_open_tier_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let https = dir.path();
        let socket = https.join("no-steward.sock");
        assert!(!restore_configured_tier(https, &socket).expect("none"));
        mark_credentials_reset(https).expect("marked");
        assert!(restore_configured_tier(https, &socket).expect("removed"));
        assert!(!https.join(OPEN_TIER_AFTER_RESET).exists());
        assert!(
            https.join(TOKEN_KEY_RETIRED).exists(),
            "the key is still replaced at the next start"
        );
        assert!(!restore_configured_tier(https, &socket).expect("again"));
    }
}
