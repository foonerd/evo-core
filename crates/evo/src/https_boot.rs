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
    build_router, install_auto_rotation, serve_https, HttpsListenerConfig,
    NoopAuditSink, ServerHandle,
};
use evo_tls_certs::{
    generate_device_ca, CertLifecycle, CertRotationPolicy, DeviceCaConfig,
    GeneratedCa, LeafConfig, RotationStats,
};
use evo_witness::WitnessChain;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
    /// Optional framework asset cache. When supplied the
    /// router additionally mounts the
    /// `/api/v1/audio/artwork/:content_hash` endpoint serving
    /// bytes from the cache for cross-node artwork
    /// propagation. Absent leaves the route unmounted; the
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
    /// Optional capability set injected on the LAN-trust
    /// admission path. Defaults to
    /// [`lan_trust_capability_set`] — the operator bootstrap
    /// set with every single-holder role scope stripped
    /// (currently `user_interaction_responder`). The floor is
    /// deliberate: LAN-trust is broad-audience (any browser on
    /// the LAN is admitted without a bearer under the Open /
    /// Secure-LAN tiers) and a single-holder role composes
    /// catastrophically with broad-audience admission — any
    /// device on the LAN would race the operator's actual UI
    /// session for the role. A vendor distribution may narrow
    /// this set further (e.g. drop `plugins:write` on a
    /// kiosk); it may NOT widen past the floor.
    pub lan_trust_capabilities: Option<evo_auth_bearer::CapabilitySet>,
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
        }
    }
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

    // 2. Bearer-token signing key: load-or-generate.
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
    //    revoke + replace it via the standard flow.
    let bootstrap_token_path = https_dir.join("bootstrap.token");
    let bootstrap_token_b64 = if !bootstrap_token_path.exists() {
        let now = now_ms();
        let capabilities = operator_bootstrap_capability_set();
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
    // Same adapter implements both Dispatcher (request/response)
    // and SubscriptionDispatcher (Subscribe-frame streaming).
    // One canonical object; no parallel adapter.
    let subscription_dispatcher: Arc<
        dyn evo_runtime_http::SubscriptionDispatcher,
    > = Arc::clone(&dispatcher)
        as Arc<dyn evo_runtime_http::SubscriptionDispatcher>;
    let schema = canonical_schema();
    let tier_provider = config
        .auth_tier_provider
        .clone()
        .unwrap_or_else(evo_runtime_http::default_auth_tier_provider);
    let lan_trust_caps = config
        .lan_trust_capabilities
        .clone()
        .unwrap_or_else(lan_trust_capability_set);
    tracing::info!(
        tier = %tier_provider.current(),
        "evo https listener: auth tier initialised",
    );
    // Persistent artwork-resolve positive index — sidecar of
    // the AssetCache root. Populated on every successful
    // resolve; consulted by the cascade FAST PATH before the
    // coalescer memo, so browse artwork on a warmed-up
    // library is O(1) per tile and survives restart.
    let artwork_resolve_index = Some(Arc::new(
        evo_runtime_http::artwork_resolve_index::ArtworkResolveIndex::new(
            config.state_dir.clone(),
        ),
    ));
    let mut router = build_router(
        &schema,
        "/api/v1",
        dispatcher,
        subscription_dispatcher,
        Arc::clone(&validator),
        NoopAuditSink::shared(),
        Some(Arc::clone(&observatory)),
        Some(Arc::clone(&witness_chain)),
        config.asset_cache.clone(),
        artwork_resolve_index,
        Arc::clone(&tier_provider),
        lan_trust_caps,
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

    // 7. Bind the HTTPS listener. When `client_ca_pem_path` is
    //    set the listener is mounted with mTLS so every TLS
    //    handshake is gated on the client presenting a cert
    //    that chains to one of the trusted CAs.
    let listener_cfg = HttpsListenerConfig::new(config.listen_addr);
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
    ])
}

/// Return the operator bootstrap capability set unioned with
/// every step-up scope declared by an admitted plugin's
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
/// Every plugin-added scope enters as `StepUp` because the
/// only manifest declaration shape that names a scope AND
/// implies operator-tier authority is `VerbCapability::StepUp`.
/// Rank fidelity is preserved: a bearer minted from this set
/// carrying `step_up:network_admin` composes with the runtime
/// dispatcher's step-up gate the same as any framework-declared
/// admin scope.
///
/// Duplicate scope names are collapsed by [`CapabilitySet`]'s
/// internal representation on iteration; the framework's
/// hardcoded entry wins on rank tie (framework already declared
/// `plugins_admin` as StepUp; a plugin re-declaring the same
/// scope is a no-op).
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
/// `StepUp` scope observed in the supplied iterator. Non-StepUp
/// variants and `VerbCapability::None` entries are ignored (only
/// step-up scopes gate operator-tier authority). Base entries
/// win on scope-name collision (a plugin re-declaring
/// `plugins_admin` produces no new entry).
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
    let mut seen: HashSet<String> = base
        .capabilities()
        .iter()
        .map(|c| c.scope().to_string())
        .collect();
    let mut merged: Vec<Capability> = base.capabilities().to_vec();
    for cap in verb_capabilities {
        if let VerbCapability::StepUp { scope } = cap {
            if seen.insert(scope.clone()) {
                merged.push(Capability::step_up(scope.as_str()));
            }
        }
    }
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

/// LAN-trust default capability set — the subset of
/// [`operator_bootstrap_capability_set`] that is safe to grant
/// to an anonymous LAN peer under Open / Secure-LAN admission.
///
/// Concretely: every scope from the operator bootstrap set,
/// minus every scope named in [`SINGLE_HOLDER_ROLE_SCOPES`].
/// Read + non-step-up-write scopes compose safely with broad-
/// audience LAN admission (multiple LAN peers can hold them
/// concurrently without disrupting each other); single-holder
/// role scopes do not, and are refused here regardless of
/// distribution config.
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
        .filter(|c| !SINGLE_HOLDER_ROLE_SCOPES.contains(&c.scope()))
        .cloned()
        .collect();
    CapabilitySet::new(filtered)
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
    fn merge_ignores_non_step_up_verb_capabilities() {
        // Only StepUp variants contribute to the merged set —
        // Read / Write / None declare per-verb access shape but
        // do not add operator-tier authority beyond what the
        // base set already carries.
        use evo_plugin_sdk::manifest::VerbCapability;
        let base = operator_bootstrap_capability_set();
        let plugin_declared = [
            VerbCapability::None,
            VerbCapability::Read {
                scope: "some_read_scope".to_string(),
            },
            VerbCapability::Write {
                scope: "some_write_scope".to_string(),
            },
        ];
        let merged =
            merge_bootstrap_with_plugin_scopes(&base, plugin_declared.iter());
        assert_eq!(
            merged.len(),
            base.len(),
            "non-StepUp variants must not change the merged set size"
        );
        let scopes: Vec<&str> =
            merged.capabilities().iter().map(|c| c.scope()).collect();
        assert!(!scopes.contains(&"some_read_scope"));
        assert!(!scopes.contains(&"some_write_scope"));
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
    fn lan_trust_set_is_operator_bootstrap_minus_single_holder_scopes() {
        // Invariant: LAN-trust set is the operator bootstrap set
        // minus every scope in SINGLE_HOLDER_ROLE_SCOPES. Guards
        // against a future edit adding a scope to the bootstrap
        // set that inadvertently widens LAN-trust.
        let bootstrap = operator_bootstrap_capability_set();
        let lan_trust = lan_trust_capability_set();
        assert_eq!(
            lan_trust.len(),
            bootstrap.len() - SINGLE_HOLDER_ROLE_SCOPES.len(),
            "LAN-trust set size drifted from bootstrap - single-holder"
        );
        let lan_scopes: std::collections::HashSet<&str> =
            lan_trust.capabilities().iter().map(|c| c.scope()).collect();
        for excluded in SINGLE_HOLDER_ROLE_SCOPES {
            assert!(
                !lan_scopes.contains(excluded),
                "LAN-trust must exclude {excluded}"
            );
        }
    }
}
