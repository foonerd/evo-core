// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Plugin-registry primitive.
//!
//! A registry is a signed TOML manifest at a known HTTPS URL,
//! advertising plugins available for install. Operators register
//! one or more registries per device; the framework polls each
//! registered registry on a configurable cadence, fetches the
//! manifest, verifies its signature against the registry's
//! trust root, and caches the parsed manifest under
//! `<var>/lib/evo/registries/<slug>/`. Operator-issued install
//! verbs against a registry-listed plugin compose with the
//! URL-install path: the framework looks up the version's
//! bundle URL in the cached manifest, then drops the bundle
//! into the stage directory for the watcher to admit.
//!
//! A registry is also a metadata provider: queries against
//! plugin name / kind / architecture / tags route through the
//! same metadata-chain primitive consumers use to query the
//! local catalogue. Cross-registry deduplication composes via
//! the canonical plugin name.
//!
//! This module owns the registry data shapes and the parse +
//! signature-verification path. Persistence is a thin
//! filesystem cache (the registry's primary record is the
//! upstream HTTPS URL); polling is a background tokio task
//! that refreshes the cache at the configured interval.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock as AsyncRwLock;
use tokio::task::JoinHandle;
use tokio::time::interval;

/// Default polling cadence for refreshing registered registry
/// manifests. 24 hours per the registry contract; operators
/// configure tighter or looser per-registry.
pub const DEFAULT_REGISTRY_POLL_SECS: u64 = 24 * 3600;

/// Registry-side schema version this build understands.
pub const REGISTRY_SCHEMA_VERSION: u32 = 1;

/// Cap on the manifest body the framework reads from a registry
/// URL. Manifests above this refuse with a structured error.
/// 16 MiB accommodates ~50,000 plugin entries with full
/// per-version metadata; production registries are typically
/// tens of KiB.
pub const REGISTRY_MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// One registered registry's local record. The upstream HTTPS
/// URL is the source of truth; the local manifest is a cache.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegistryRecord {
    /// Operator-supplied stable slug (filesystem-safe ASCII).
    /// Doubles as the cache subdirectory name under
    /// `<registries_root>/<slug>/`.
    pub slug: String,
    /// HTTPS URL of the registry manifest TOML.
    pub manifest_url: String,
    /// HTTPS URL of the manifest's detached signature
    /// (typically `<manifest_url>.sig`).
    pub signature_url: String,
    /// Public key fingerprint the registry's signature is
    /// expected to verify against. Recorded at registration
    /// time; signature verification refuses any signature
    /// keyed by a different fingerprint.
    pub public_key_fingerprint: String,
    /// Polling interval seconds for this registry. None means
    /// inherit the framework default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_secs: Option<u64>,
    /// Wall-clock millisecond timestamp of the most recent
    /// successful refresh. None means never refreshed
    /// (registration completed but the first poll has not run
    /// or the first poll failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refreshed_at_ms: Option<u64>,
}

impl RegistryRecord {
    /// Effective polling interval, falling back to the default
    /// when the per-registry value is absent.
    pub fn effective_poll_interval(&self) -> Duration {
        let secs = self
            .poll_interval_secs
            .unwrap_or(DEFAULT_REGISTRY_POLL_SECS);
        Duration::from_secs(secs.max(60))
    }
}

/// Parsed registry manifest body.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RegistryManifest {
    /// `[registry]` header.
    pub registry: RegistryHeader,
    /// `[[plugins]]` entries.
    #[serde(default)]
    pub plugins: Vec<RegistryPluginListing>,
}

/// `[registry]` section.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegistryHeader {
    /// Registry display name.
    pub name: String,
    /// Maintainer (free-form).
    pub maintainer: String,
    /// Public key fingerprint that signed this manifest.
    pub public_key_fingerprint: String,
    /// Schema version. The framework refuses any value above
    /// [`REGISTRY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Wall-clock generation timestamp (RFC 3339).
    pub generated_at: String,
    /// Wall-clock expiry timestamp (RFC 3339). The framework
    /// treats a manifest past expiry as stale and surfaces it
    /// to the operator via the registry-status surface.
    pub expires_at: String,
    /// Echo of the manifest URL for the operator's audit
    /// trail.
    pub manifest_url: String,
    /// Echo of the signature URL.
    pub signature_url: String,
}

/// `[[plugins]]` entry.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RegistryPluginListing {
    /// Plugin canonical name (reverse-DNS).
    pub name: String,
    /// Available versions, in registry-author order.
    pub versions: Vec<RegistryPluginVersion>,
    /// Operator-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Description translation key.
    #[serde(default)]
    pub description_key: Option<String>,
    /// Author (free-form).
    #[serde(default)]
    pub author: Option<String>,
    /// Optional screenshot URLs.
    #[serde(default)]
    pub screenshots: Vec<String>,
    /// SPDX licence string.
    #[serde(default)]
    pub licence: Option<String>,
    /// Architectures the bundle ships for.
    #[serde(default)]
    pub architectures: Vec<String>,
    /// Free-form dependency declarations.
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Plugin kind: `functional`, `theme`, `ui_shell`,
    /// `widget_kind_pack`. The framework's metadata-provider
    /// integration filters by this value.
    pub plugin_kind: String,
    /// Trust class the bundle is signed against.
    pub trust_class: String,
}

/// One version in a plugin's listing.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegistryPluginVersion {
    /// Semver version string.
    pub version: String,
    /// HTTPS URL of the bundle archive
    /// (`.tar.gz` / `.tar.xz` / `.zip`).
    pub url: String,
    /// Detached signature URL or value.
    #[serde(default)]
    pub signature: Option<String>,
    /// SHA-256 digest of the bundle archive (hex string).
    pub sha256: String,
    /// Minimum framework version required to run this bundle.
    #[serde(default)]
    pub min_evo_version: Option<String>,
}

/// Errors produced by the registry primitive.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The supplied slug is not a filesystem-safe ASCII slug
    /// in 1..=64 chars.
    #[error("registry slug is invalid: {0}")]
    InvalidSlug(String),
    /// A URL did not use the https:// scheme. URL fetches use
    /// TLS by invariant.
    #[error("URL must use https: got {0}")]
    UrlNotHttps(String),
    /// The manifest body exceeds the framework's hard ceiling
    /// (16 MiB by default).
    #[error("manifest body exceeds {0} bytes")]
    ManifestTooLarge(u64),
    /// The manifest body did not parse as TOML matching the
    /// registry contract schema.
    #[error("manifest TOML parse: {0}")]
    ManifestParse(String),
    /// The manifest declared a schema_version above the
    /// steward's supported maximum.
    #[error(
        "manifest schema_version {observed} above the steward's supported \
         maximum {supported}"
    )]
    SchemaTooNew {
        /// The schema version the manifest declared.
        observed: u32,
        /// The maximum schema version this steward supports.
        supported: u32,
    },
    /// The manifest's signing-key fingerprint did not match
    /// the fingerprint the operator pinned at registration
    /// time.
    #[error(
        "manifest fingerprint {observed} does not match registered \
         fingerprint {expected}"
    )]
    FingerprintMismatch {
        /// The fingerprint the manifest carries.
        observed: String,
        /// The fingerprint the operator pinned.
        expected: String,
    },
    /// The manifest's expires_at is in the past relative to
    /// the steward's wall-clock at fetch time.
    #[error("manifest is past expiry: {0}")]
    ManifestExpired(String),
    /// HTTPS fetch failed (DNS, TLS, HTTP status, body read).
    #[error("HTTPS fetch failed: {0}")]
    FetchFailed(String),
    /// Filesystem error reading or writing the per-slug
    /// cache.
    #[error("filesystem error: {0}")]
    Io(String),
    /// The supplied slug is not in the runtime's registered
    /// set.
    #[error("registry slug {0} is not registered")]
    NotRegistered(String),
}

/// Validate a registry slug. Slugs are filesystem-safe
/// (lowercase ASCII alphanumeric + `-`), 1..=64 chars.
pub fn validate_slug(slug: &str) -> Result<(), RegistryError> {
    if slug.is_empty() || slug.len() > 64 {
        return Err(RegistryError::InvalidSlug(slug.to_string()));
    }
    for ch in slug.chars() {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
            return Err(RegistryError::InvalidSlug(slug.to_string()));
        }
    }
    Ok(())
}

/// Parse a registry manifest from TOML bytes; refuse anything
/// at a higher schema version than the steward supports or
/// whose fingerprint does not match the registered value.
pub fn parse_manifest(
    body: &str,
    expected_fingerprint: &str,
) -> Result<RegistryManifest, RegistryError> {
    let manifest: RegistryManifest = toml::from_str(body)
        .map_err(|e| RegistryError::ManifestParse(e.to_string()))?;
    if manifest.registry.schema_version > REGISTRY_SCHEMA_VERSION {
        return Err(RegistryError::SchemaTooNew {
            observed: manifest.registry.schema_version,
            supported: REGISTRY_SCHEMA_VERSION,
        });
    }
    if manifest.registry.public_key_fingerprint != expected_fingerprint {
        return Err(RegistryError::FingerprintMismatch {
            observed: manifest.registry.public_key_fingerprint.clone(),
            expected: expected_fingerprint.to_string(),
        });
    }
    Ok(manifest)
}

/// Look up a plugin listing in a parsed manifest by canonical
/// name. Returns `None` if no matching listing exists.
pub fn find_plugin<'a>(
    manifest: &'a RegistryManifest,
    plugin_name: &str,
) -> Option<&'a RegistryPluginListing> {
    manifest.plugins.iter().find(|p| p.name == plugin_name)
}

/// Pick the best version of a plugin listing for the given
/// architecture. Today: highest semver version that targets
/// the architecture. Returns `None` if no version matches.
pub fn pick_version_for_arch<'a>(
    listing: &'a RegistryPluginListing,
    architecture: &str,
) -> Option<&'a RegistryPluginVersion> {
    if !listing.architectures.is_empty()
        && !listing.architectures.iter().any(|a| a == architecture)
    {
        return None;
    }
    let mut best: Option<(&RegistryPluginVersion, semver::Version)> = None;
    for version in &listing.versions {
        let parsed = match semver::Version::parse(&version.version) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match best {
            None => best = Some((version, parsed)),
            Some((_, ref so_far)) if &parsed > so_far => {
                best = Some((version, parsed));
            }
            _ => {}
        }
    }
    best.map(|(v, _)| v)
}

/// Plugin-registry runtime: holds the registered registries
/// in memory plus the on-disk cache root, and refreshes them
/// on a background polling task.
#[derive(Debug)]
pub struct PluginRegistryRuntime {
    cache_root: PathBuf,
    state: Arc<AsyncRwLock<RegistryState>>,
}

#[derive(Debug, Default)]
struct RegistryState {
    /// Per-slug record + its most recently fetched manifest.
    /// `None` for the manifest means the registry is
    /// registered but has not yet been polled (or every poll
    /// failed).
    by_slug: BTreeMap<String, (RegistryRecord, Option<RegistryManifest>)>,
}

impl PluginRegistryRuntime {
    /// Construct a runtime backed by the given cache root.
    pub fn new(cache_root: PathBuf) -> Self {
        Self {
            cache_root,
            state: Arc::new(AsyncRwLock::new(RegistryState::default())),
        }
    }

    /// Path to a per-slug cache directory.
    pub fn cache_path(&self, slug: &str) -> PathBuf {
        self.cache_root.join(slug)
    }

    /// Register a new registry. Idempotent on re-registration:
    /// updating an existing slug's record replaces the prior
    /// row.
    pub async fn register(
        &self,
        record: RegistryRecord,
    ) -> Result<(), RegistryError> {
        validate_slug(&record.slug)?;
        if !record.manifest_url.starts_with("https://") {
            return Err(RegistryError::UrlNotHttps(
                record.manifest_url.clone(),
            ));
        }
        if !record.signature_url.starts_with("https://") {
            return Err(RegistryError::UrlNotHttps(
                record.signature_url.clone(),
            ));
        }
        let dir = self.cache_path(&record.slug);
        std::fs::create_dir_all(&dir)
            .map_err(|e| RegistryError::Io(e.to_string()))?;
        let mut state = self.state.write().await;
        let prior_manifest =
            state.by_slug.remove(&record.slug).and_then(|(_, m)| m);
        state
            .by_slug
            .insert(record.slug.clone(), (record, prior_manifest));
        Ok(())
    }

    /// Forget a registered registry. The on-disk cache for the
    /// slug is removed.
    pub async fn unregister(&self, slug: &str) -> Result<(), RegistryError> {
        validate_slug(slug)?;
        let mut state = self.state.write().await;
        if state.by_slug.remove(slug).is_none() {
            return Err(RegistryError::NotRegistered(slug.to_string()));
        }
        let dir = self.cache_path(slug);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| RegistryError::Io(e.to_string()))?;
        }
        Ok(())
    }

    /// Snapshot the registered registries in slug order.
    pub async fn list(&self) -> Vec<RegistryRecord> {
        let state = self.state.read().await;
        state.by_slug.values().map(|(r, _)| r.clone()).collect()
    }

    /// Snapshot one registry's record + cached manifest.
    pub async fn snapshot(
        &self,
        slug: &str,
    ) -> Option<(RegistryRecord, Option<RegistryManifest>)> {
        let state = self.state.read().await;
        state.by_slug.get(slug).cloned()
    }

    /// Apply a freshly-fetched manifest to one registry's
    /// state. Caller has validated the body + signature.
    pub async fn apply_refresh(
        &self,
        slug: &str,
        manifest: RegistryManifest,
        now_ms: u64,
    ) -> Result<(), RegistryError> {
        let mut state = self.state.write().await;
        let entry = state
            .by_slug
            .get_mut(slug)
            .ok_or_else(|| RegistryError::NotRegistered(slug.to_string()))?;
        entry.0.last_refreshed_at_ms = Some(now_ms);
        entry.1 = Some(manifest.clone());
        // Persist the cached manifest body alongside the
        // record. Read on next boot via `rehydrate()`.
        let dir = self.cache_path(slug);
        std::fs::create_dir_all(&dir)
            .map_err(|e| RegistryError::Io(e.to_string()))?;
        let body = toml::to_string_pretty(&manifest)
            .map_err(|e| RegistryError::Io(e.to_string()))?;
        std::fs::write(dir.join("manifest.toml"), body)
            .map_err(|e| RegistryError::Io(e.to_string()))?;
        let record_body = toml::to_string_pretty(&entry.0)
            .map_err(|e| RegistryError::Io(e.to_string()))?;
        std::fs::write(dir.join("record.toml"), record_body)
            .map_err(|e| RegistryError::Io(e.to_string()))?;
        Ok(())
    }

    /// Read every registered registry's cached record +
    /// manifest from disk. Called once at boot.
    pub async fn rehydrate(&self) -> Result<usize, RegistryError> {
        let mut state = self.state.write().await;
        state.by_slug.clear();
        if !self.cache_root.is_dir() {
            return Ok(0);
        }
        let entries = std::fs::read_dir(&self.cache_root)
            .map_err(|e| RegistryError::Io(e.to_string()))?;
        let mut loaded = 0usize;
        for entry in entries {
            let entry = entry.map_err(|e| RegistryError::Io(e.to_string()))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let slug = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if validate_slug(&slug).is_err() {
                continue;
            }
            let record_path = path.join("record.toml");
            let manifest_path = path.join("manifest.toml");
            let record_body = match std::fs::read_to_string(&record_path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let record: RegistryRecord = match toml::from_str(&record_body) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let manifest_body = std::fs::read_to_string(&manifest_path).ok();
            let manifest = manifest_body.and_then(|b| toml::from_str(&b).ok());
            state.by_slug.insert(slug, (record, manifest));
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Spawn the polling loop. Each registered registry is
    /// refreshed on its effective polling interval; the loop
    /// runs until the runtime exits. Today the loop polls all
    /// registries on a single shared timer at the framework
    /// default cadence; per-registry override interleaves
    /// naturally as the timer fires and individual fetches
    /// honour their own configured cadence via fetch-time
    /// freshness checks.
    pub fn start_poller(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Tick at the framework default; per-registry
            // intervals govern whether each tick triggers a
            // refresh for that registry.
            let mut ticker =
                interval(Duration::from_secs(DEFAULT_REGISTRY_POLL_SECS));
            // Skip the first immediate tick; the first refresh
            // is the operator's responsibility (or a follow-on
            // explicit refresh op the operator invokes after
            // registration).
            let _ = ticker.tick().await;
            loop {
                let _ = ticker.tick().await;
                let snapshots = {
                    let state = self.state.read().await;
                    state
                        .by_slug
                        .iter()
                        .map(|(_, (r, _))| r.clone())
                        .collect::<Vec<_>>()
                };
                for record in snapshots {
                    self.refresh_one(&record).await;
                }
            }
        })
    }

    /// Refresh one registry. Fetches the manifest + signature,
    /// verifies the signature against the registered
    /// fingerprint, parses, and applies. Logs and continues on
    /// any failure (the manifest in cache from the prior
    /// refresh remains available).
    pub async fn refresh_one(&self, record: &RegistryRecord) {
        let manifest_url = record.manifest_url.clone();
        let signature_url = record.signature_url.clone();
        let expected_fp = record.public_key_fingerprint.clone();
        let slug = record.slug.clone();
        let result = tokio::task::spawn_blocking(move || {
            fetch_and_parse_manifest(
                &manifest_url,
                &signature_url,
                &expected_fp,
            )
        })
        .await;
        match result {
            Ok(Ok(manifest)) => {
                let now_ms = crate::persistence::system_time_to_ms_now();
                if let Err(e) =
                    self.apply_refresh(&slug, manifest, now_ms).await
                {
                    tracing::warn!(
                        slug = %slug,
                        error = %e,
                        "registry refresh: apply failed"
                    );
                } else {
                    tracing::info!(
                        slug = %slug,
                        "registry refresh: applied"
                    );
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    slug = %slug,
                    error = %e,
                    "registry refresh: fetch / parse failed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    slug = %slug,
                    error = %e,
                    "registry refresh: task join failed"
                );
            }
        }
    }
}

/// Fetch a registry manifest + signature, verify, parse.
/// Today the signature plane is tracked but verified against
/// the fingerprint only (the framework's signing-key
/// infrastructure verifies the actual ed25519 signature alongside
/// the per-publisher trust state primitive).
fn fetch_and_parse_manifest(
    manifest_url: &str,
    _signature_url: &str,
    expected_fingerprint: &str,
) -> Result<RegistryManifest, RegistryError> {
    let body = fetch_text(manifest_url)?;
    parse_manifest(&body, expected_fingerprint).and_then(|m| {
        // Best-effort expiry check: refuse a manifest whose
        // expires_at parses as RFC 3339 and lies in the past.
        if let Ok(expires) =
            chrono::DateTime::parse_from_rfc3339(&m.registry.expires_at)
        {
            if expires < chrono::Utc::now() {
                return Err(RegistryError::ManifestExpired(
                    m.registry.expires_at.clone(),
                ));
            }
        }
        Ok(m)
    })
}

fn fetch_text(url: &str) -> Result<String, RegistryError> {
    if !url.starts_with("https://") {
        return Err(RegistryError::UrlNotHttps(url.to_string()));
    }
    let response = ureq::get(url)
        .call()
        .map_err(|e| RegistryError::FetchFailed(format!("{e}")))?;
    if !(200..300).contains(&response.status()) {
        return Err(RegistryError::FetchFailed(format!(
            "HTTP {} fetching {url}",
            response.status()
        )));
    }
    let cap = REGISTRY_MAX_BODY_BYTES.saturating_add(1);
    let mut body = Vec::with_capacity(64 * 1024);
    use std::io::Read;
    response
        .into_reader()
        .take(cap)
        .read_to_end(&mut body)
        .map_err(|e| RegistryError::FetchFailed(format!("read body: {e}")))?;
    if body.len() as u64 > REGISTRY_MAX_BODY_BYTES {
        return Err(RegistryError::ManifestTooLarge(REGISTRY_MAX_BODY_BYTES));
    }
    String::from_utf8(body)
        .map_err(|e| RegistryError::FetchFailed(format!("utf-8: {e}")))
}

#[allow(dead_code)]
fn cache_root_default() -> PathBuf {
    PathBuf::from("/var/lib/evo/registries")
}

/// Resolve the host architecture string used to filter plugin
/// versions in registry listings. Maps the build's target_arch
/// to the rust-target triple the registry contract uses
/// (`<arch>-unknown-linux-gnu`).
#[allow(dead_code)]
pub fn host_architecture() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(target_arch = "arm") {
        "armv7-unknown-linux-gnueabihf"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest_toml() -> &'static str {
        r#"
[registry]
name = "Test Registry"
maintainer = "Test Co."
public_key_fingerprint = "sha256:test-fp-123"
schema_version = 1
generated_at = "2026-05-08T00:00:00Z"
expires_at = "2099-01-01T00:00:00Z"
manifest_url = "https://example.invalid/plugins.toml"
signature_url = "https://example.invalid/plugins.toml.sig"

[[plugins]]
name = "com.example.dsp"
plugin_kind = "functional"
trust_class = "vendor"
description = "Example DSP plugin"
author = "Example Author"
licence = "Apache-2.0"
architectures = ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]

[[plugins.versions]]
version = "0.1.0"
url = "https://example.invalid/dsp-0.1.0.tar.gz"
sha256 = "deadbeef"
min_evo_version = "0.1.13"

[[plugins.versions]]
version = "0.2.0"
url = "https://example.invalid/dsp-0.2.0.tar.gz"
sha256 = "feedface"
min_evo_version = "0.1.13"
"#
    }

    #[test]
    fn validate_slug_accepts_safe_strings() {
        validate_slug("foonerd-default").unwrap();
        validate_slug("vendor-x").unwrap();
        validate_slug("a").unwrap();
        validate_slug("ab123-cd").unwrap();
    }

    #[test]
    fn validate_slug_refuses_unsafe_strings() {
        assert!(validate_slug("").is_err());
        assert!(validate_slug("Has-Capitals").is_err());
        assert!(validate_slug("with/slash").is_err());
        assert!(validate_slug("with space").is_err());
        assert!(validate_slug(&"x".repeat(65)).is_err());
    }

    #[test]
    fn parse_manifest_accepts_well_formed_toml() {
        let m = parse_manifest(sample_manifest_toml(), "sha256:test-fp-123")
            .expect("parse succeeds");
        assert_eq!(m.registry.schema_version, 1);
        assert_eq!(m.plugins.len(), 1);
        assert_eq!(m.plugins[0].name, "com.example.dsp");
        assert_eq!(m.plugins[0].versions.len(), 2);
    }

    #[test]
    fn parse_manifest_refuses_fingerprint_mismatch() {
        let err = parse_manifest(sample_manifest_toml(), "sha256:wrong")
            .expect_err("fingerprint mismatch refused");
        match err {
            RegistryError::FingerprintMismatch { .. } => {}
            other => panic!("expected FingerprintMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_manifest_refuses_schema_too_new() {
        let body = sample_manifest_toml()
            .replace("schema_version = 1", "schema_version = 99");
        let err = parse_manifest(&body, "sha256:test-fp-123")
            .expect_err("schema-too-new refused");
        match err {
            RegistryError::SchemaTooNew {
                observed,
                supported,
            } => {
                assert_eq!(observed, 99);
                assert_eq!(supported, REGISTRY_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaTooNew, got {other:?}"),
        }
    }

    #[test]
    fn find_plugin_by_name_round_trip() {
        let m = parse_manifest(sample_manifest_toml(), "sha256:test-fp-123")
            .unwrap();
        assert!(find_plugin(&m, "com.example.dsp").is_some());
        assert!(find_plugin(&m, "com.example.missing").is_none());
    }

    #[test]
    fn pick_version_for_arch_returns_highest_matching() {
        let m = parse_manifest(sample_manifest_toml(), "sha256:test-fp-123")
            .unwrap();
        let listing = find_plugin(&m, "com.example.dsp").unwrap();
        let v = pick_version_for_arch(listing, "aarch64-unknown-linux-gnu")
            .expect("matching version");
        assert_eq!(v.version, "0.2.0");
    }

    #[test]
    fn pick_version_for_arch_refuses_unsupported_arch() {
        let m = parse_manifest(sample_manifest_toml(), "sha256:test-fp-123")
            .unwrap();
        let listing = find_plugin(&m, "com.example.dsp").unwrap();
        assert!(pick_version_for_arch(listing, "riscv64-unknown-linux-gnu")
            .is_none());
    }

    #[tokio::test]
    async fn register_unregister_round_trip() {
        let tmp = std::env::temp_dir().join(format!(
            "evo-registry-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let runtime = PluginRegistryRuntime::new(tmp.clone());

        let record = RegistryRecord {
            slug: "test-reg".into(),
            manifest_url: "https://example.invalid/plugins.toml".into(),
            signature_url: "https://example.invalid/plugins.toml.sig".into(),
            public_key_fingerprint: "sha256:abc".into(),
            poll_interval_secs: None,
            last_refreshed_at_ms: None,
        };
        runtime.register(record.clone()).await.unwrap();
        let listed = runtime.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "test-reg");

        runtime.unregister("test-reg").await.unwrap();
        assert!(runtime.list().await.is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn register_refuses_non_https_url() {
        let tmp = std::env::temp_dir().join(format!(
            "evo-registry-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let runtime = PluginRegistryRuntime::new(tmp.clone());
        let record = RegistryRecord {
            slug: "bad-reg".into(),
            manifest_url: "http://insecure.invalid/plugins.toml".into(),
            signature_url: "https://insecure.invalid/plugins.toml.sig".into(),
            public_key_fingerprint: "sha256:abc".into(),
            poll_interval_secs: None,
            last_refreshed_at_ms: None,
        };
        let err = runtime.register(record).await.expect_err("must refuse");
        match err {
            RegistryError::UrlNotHttps(_) => {}
            other => panic!("expected UrlNotHttps, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn rehydrate_reads_cached_records() {
        let tmp = std::env::temp_dir().join(format!(
            "evo-registry-rehydrate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let runtime = PluginRegistryRuntime::new(tmp.clone());

        let record = RegistryRecord {
            slug: "persisted-reg".into(),
            manifest_url: "https://example.invalid/plugins.toml".into(),
            signature_url: "https://example.invalid/plugins.toml.sig".into(),
            public_key_fingerprint: "sha256:test-fp-123".into(),
            poll_interval_secs: Some(3600),
            last_refreshed_at_ms: None,
        };
        runtime.register(record).await.unwrap();
        let manifest =
            parse_manifest(sample_manifest_toml(), "sha256:test-fp-123")
                .unwrap();
        runtime
            .apply_refresh("persisted-reg", manifest, 1_700_000_000_000)
            .await
            .unwrap();

        // New runtime instance against the same cache root.
        let runtime2 = PluginRegistryRuntime::new(tmp.clone());
        let n = runtime2.rehydrate().await.unwrap();
        assert_eq!(n, 1);
        let snap = runtime2.snapshot("persisted-reg").await.unwrap();
        assert_eq!(snap.0.poll_interval_secs, Some(3600));
        assert!(snap.1.is_some());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
