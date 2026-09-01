// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Core (steward) update source — the framework-default
//! `"core"` source.
//!
//! Fetches a release-channel `build-info.toml` + signature
//! pair, verifies the signature against the device's
//! release-trust roots, parses the manifest, and surfaces the
//! advertised steward binary version as an
//! [`UpdateAvailable`] when it is newer than the running
//! steward. `apply_update` fetches the signed binary, verifies
//! its release-key signature, stages it to a writable path,
//! then triggers a graceful steward restart (sub-primitive J)
//! pointing at the staged binary.
//!
//! ## Channel layout
//!
//! The source assumes the channel publishes one tree per
//! target triple, mirroring `foonerd/evo-core-artefacts`:
//!
//! ```text
//! <channel_base>/
//!   <target>/
//!     build-info.toml      ← signed manifest (TOML)
//!     build-info.sig       ← raw 64-byte ed25519 signature
//!     evo                  ← steward binary
//!     evo.sig              ← raw 64-byte ed25519 signature
//!     evo.sha256           ← hex digest (advisory; signature is the trust gate)
//! ```
//!
//! The manifest's `kind = "core-binaries"` field selects the
//! [`evo_trust::ReleaseRole::FrameworkRelease`] role for
//! signature verification per
//! [`evo_trust::role_for_artefact_kind`].
//!
//! ## Current substrate scope
//!
//! Sub-primitive I2 of the three-channel update model. Out of
//! scope, named explicitly:
//!
//! - **Updating `evo-plugin-tool` alongside `evo`** —
//!   operator tooling is published in the same channel but
//!   the substrate apply path replaces `evo` only. The
//!   tooling rides a follow-on iteration that handles
//!   non-steward-binary artefacts.
//! - **Auxiliary file installation** (systemd unit etc.) —
//!   packaging is distribution-owned per BOUNDARY.md §6.
//! - **Rollback / A/B staging** — `requires_restart=Steward`
//!   plus signature-verified swap is the substrate floor;
//!   rollback rides follow-on.
//! - **Delta / binary-diff fetches** — full-binary fetch is
//!   the floor; sources with delta capability replace this
//!   one.
//! - **Channel URL signing / pinning** — the
//!   release-artefact signature is the trust gate today;
//!   signed channel pointers ride follow-on.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use evo_plugin_sdk::update::{
    ApplyOptions, RestartLevel, SourceCapabilities, SourceId, UpdateAvailable,
    UpdateError, UpdateId, UpdateOutcome, UpdateSeverity, UpdateSource,
};
use evo_trust::{
    role_for_artefact_kind, verify_release_signature, ReleaseTrustKey,
};
use serde::Deserialize;

use crate::happenings::HappeningBus;
use crate::plugin_registry::host_architecture;
use crate::restart::{RestartCoordinator, RestartRequest};

/// Maximum bytes the source will read for the `build-info`
/// manifest. The real manifests are <4 KiB; the cap is a
/// generous bound rather than a tight fit.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Maximum bytes the source will fetch for a steward
/// binary. The current `evo` release is ~30 MiB; 128 MiB
/// gives headroom for a future debug-symbols-included build
/// without inviting unbounded growth.
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;

/// Maximum bytes the source will read for a signature
/// sidecar. Raw ed25519 signatures are 64 bytes; the cap is
/// a sanity bound only.
const MAX_SIG_BYTES: u64 = 4 * 1024;

/// HTTP request timeout for one fetch.
const FETCH_TIMEOUT_SECS: u64 = 120;

/// Source-id the framework expects for the `"core"` source.
pub const SOURCE_ID: &str = "core";

/// Release-channel manifest shape. Mirrors the
/// `build-info.toml` produced by the framework's release
/// pipeline (see `binaries/<target>/build-info.toml` in
/// `foonerd/evo-core-artefacts`).
///
/// The fields the source reads are the version tag and the
/// declared kind; everything else is ignored at this level
/// (auxiliary files, publisher identity, build timestamp) but
/// admitted unknown fields are accepted so a manifest with
/// future-added optional fields still parses.
#[derive(Debug, Clone, Deserialize)]
pub struct CoreReleaseManifest {
    /// Manifest schema version. Currently `0`.
    #[serde(default)]
    pub schema_version: u32,
    /// Artefact kind (e.g. `core-binaries`). Selects the
    /// release role used for signature verification.
    pub kind: String,
    /// The release tag (e.g. `vMAJOR.MINOR.PATCH`). The source strips
    /// the leading `v` and parses the remainder as semver.
    pub evo_core_tag: String,
    /// The target triple this manifest applies to (e.g.
    /// `aarch64-unknown-linux-gnu`).
    pub target: String,
    /// Names of binaries the channel hosts (the source only
    /// fetches `evo`).
    #[serde(default)]
    pub binaries: Vec<String>,
}

/// Core update source.
pub struct CoreUpdateSource {
    /// Base channel URL. The source appends
    /// `/<target>/<file>` to this.
    channel_base: String,
    /// Target triple this device matches against.
    target: &'static str,
    /// Currently-running steward semver string.
    current_version: &'static str,
    /// Release-trust roots loaded at boot. Empty means the
    /// device admits no release-channel updates.
    release_trust: Arc<Vec<ReleaseTrustKey>>,
    /// Stage directory the fetched binary lands in before
    /// the graceful restart picks it up.
    stage_dir: PathBuf,
    /// Restart coordinator the apply path triggers.
    restart_coordinator: Arc<RestartCoordinator>,
    /// Happenings bus — kept for future emission of
    /// fetch-progress / verify-failed signals on the
    /// integration iteration.
    #[allow(dead_code)]
    happenings: Arc<HappeningBus>,
}

impl std::fmt::Debug for CoreUpdateSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreUpdateSource")
            .field("channel_base", &self.channel_base)
            .field("target", &self.target)
            .field("current_version", &self.current_version)
            .field("stage_dir", &self.stage_dir)
            .field("release_trust_keys", &self.release_trust.len())
            .finish_non_exhaustive()
    }
}

impl CoreUpdateSource {
    /// Construct a source. The caller passes the configured
    /// channel URL, the loaded release-trust roots, the
    /// staging directory, and the restart coordinator the
    /// apply path triggers.
    pub fn new(
        channel_base: String,
        release_trust: Arc<Vec<ReleaseTrustKey>>,
        stage_dir: PathBuf,
        restart_coordinator: Arc<RestartCoordinator>,
        happenings: Arc<HappeningBus>,
    ) -> Self {
        Self {
            channel_base: trim_trailing_slashes(&channel_base),
            target: host_architecture(),
            current_version: env!("CARGO_PKG_VERSION"),
            release_trust,
            stage_dir,
            restart_coordinator,
            happenings,
        }
    }

    /// Compose the URL for one channel artefact (manifest,
    /// signature, or binary).
    fn url_for(&self, file: &str) -> String {
        format!("{}/{}/{}", self.channel_base, self.target, file)
    }

    /// Fetch + verify + parse the manifest. Returns
    /// `Err(SourceUnreachable)` when the channel cannot be
    /// read; `Err(SignatureInvalid)` when the manifest
    /// signature does not verify against any release-trust
    /// key with the matching role; `Err(Internal)` when the
    /// manifest's declared kind has no role mapping.
    async fn fetch_and_verify_manifest(
        &self,
    ) -> Result<CoreReleaseManifest, UpdateError> {
        let manifest_url = self.url_for("build-info.toml");
        let sig_url = self.url_for("build-info.sig");
        let trust = Arc::clone(&self.release_trust);
        tokio::task::spawn_blocking(move || {
            let manifest_bytes =
                fetch_bounded(&manifest_url, MAX_MANIFEST_BYTES)?;
            let sig_bytes = fetch_bounded(&sig_url, MAX_SIG_BYTES)?;
            let manifest_text =
                std::str::from_utf8(&manifest_bytes).map_err(|e| {
                    UpdateError::Internal(format!(
                        "manifest is not valid utf-8: {e}"
                    ))
                })?;
            let parsed: CoreReleaseManifest = toml::from_str(manifest_text)
                .map_err(|e| {
                    UpdateError::Internal(format!("manifest parse failed: {e}"))
                })?;
            let role =
                role_for_artefact_kind(&parsed.kind).ok_or_else(|| {
                    UpdateError::Internal(format!(
                        "manifest kind {:?} has no release role",
                        parsed.kind
                    ))
                })?;
            verify_release_signature(&manifest_bytes, &sig_bytes, role, &trust)
                .map_err(|e| {
                    UpdateError::SignatureInvalid(format!(
                        "build-info signature verify failed: {e}"
                    ))
                })?;
            Ok(parsed)
        })
        .await
        .map_err(|e| {
            UpdateError::Internal(format!("manifest task join failed: {e}"))
        })?
    }

    /// True when `available` strictly dominates `current`
    /// under semver. Conservative: non-parseable strings
    /// refuse the upgrade.
    pub fn is_newer(current: &str, available: &str) -> bool {
        let a = strip_v_prefix(available);
        let c = strip_v_prefix(current);
        match (semver::Version::parse(c), semver::Version::parse(a)) {
            (Ok(cv), Ok(av)) => av > cv,
            _ => false,
        }
    }

    /// Aggregate the `check_for_updates` outcome. Returns
    /// `Ok(vec)` when the manifest verified; the vector is
    /// empty when no upgrade is available.
    async fn collect_updates(
        &self,
    ) -> Result<Vec<UpdateAvailable>, UpdateError> {
        if self.release_trust.is_empty() {
            return Err(UpdateError::Internal(
                "core update source: release trust set is empty".to_string(),
            ));
        }
        let manifest = self.fetch_and_verify_manifest().await?;
        if manifest.target != self.target {
            return Err(UpdateError::Internal(format!(
                "manifest target {:?} does not match host target {:?}",
                manifest.target, self.target
            )));
        }
        let available = strip_v_prefix(&manifest.evo_core_tag).to_string();
        if !Self::is_newer(self.current_version, &available) {
            return Ok(Vec::new());
        }
        Ok(vec![UpdateAvailable {
            id: UpdateId::new(format!("evo@{}", available)),
            component: "evo".to_string(),
            current_version: self.current_version.to_string(),
            available_version: available,
            changelog_url: None,
            severity: UpdateSeverity::Routine,
            size_bytes: None,
            requires_restart: RestartLevel::Steward,
            published_at: std::time::SystemTime::UNIX_EPOCH,
        }])
    }

    /// Fetch + verify + stage the steward binary. Returns
    /// the staged path on success.
    async fn fetch_verify_stage_binary(
        &self,
        target_version: &str,
    ) -> Result<PathBuf, UpdateError> {
        let binary_url = self.url_for("evo");
        let sig_url = self.url_for("evo.sig");
        let trust = Arc::clone(&self.release_trust);
        let stage_dir = self.stage_dir.clone();
        let target_version = target_version.to_string();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&stage_dir).map_err(|e| {
                UpdateError::ApplyFailed(format!(
                    "creating stage dir {}: {e}",
                    stage_dir.display()
                ))
            })?;
            let binary_bytes = fetch_bounded(&binary_url, MAX_BINARY_BYTES)?;
            let sig_bytes = fetch_bounded(&sig_url, MAX_SIG_BYTES)?;
            verify_release_signature(
                &binary_bytes,
                &sig_bytes,
                evo_trust::ReleaseRole::FrameworkRelease,
                &trust,
            )
            .map_err(|e| {
                UpdateError::SignatureInvalid(format!(
                    "binary signature verify failed: {e}"
                ))
            })?;
            let safe_version = sanitise_filename(&target_version);
            let staged_path = stage_dir.join(format!("evo-{safe_version}"));
            write_executable(&staged_path, &binary_bytes)?;
            Ok(staged_path)
        })
        .await
        .map_err(|e| {
            UpdateError::Internal(format!("binary task join failed: {e}"))
        })?
    }
}

impl UpdateSource for CoreUpdateSource {
    fn source_id(&self) -> SourceId {
        SourceId::new(SOURCE_ID)
    }

    fn display_name(&self) -> String {
        "Core (steward) updates".to_string()
    }

    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            background_check: true,
            atomic_apply: true,
            requires_restart: RestartLevel::Steward,
            rollback_supported: false,
            size_estimate: false,
        }
    }

    fn check_for_updates<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<UpdateAvailable>, UpdateError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.collect_updates().await })
    }

    fn apply_update<'a>(
        &'a self,
        id: &'a UpdateId,
        options: &'a ApplyOptions,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<UpdateOutcome, UpdateError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let raw = id.as_str();
            let (component, target_version) =
                raw.rsplit_once('@').ok_or_else(|| {
                    UpdateError::UnknownUpdate(format!(
                        "malformed update id: {raw}"
                    ))
                })?;
            if component != "evo" {
                return Err(UpdateError::UnknownUpdate(format!(
                    "core source only updates evo, not {component:?}"
                )));
            }
            // Re-check the manifest to confirm the requested
            // version is still the channel's current best.
            // This refuses applies that race against a
            // channel rollback.
            let manifest = self.fetch_and_verify_manifest().await?;
            let advertised = strip_v_prefix(&manifest.evo_core_tag);
            if advertised != target_version {
                return Err(UpdateError::UnknownUpdate(format!(
                    "channel advertises evo@{advertised}, request was \
                     evo@{target_version}"
                )));
            }
            if options.dry_run {
                // Dry-run: manifest verified, version
                // matches; do not fetch the binary or
                // restart.
                return Ok(UpdateOutcome {
                    id: id.clone(),
                    component: component.to_string(),
                    applied_version: target_version.to_string(),
                    restart_initiated: RestartLevel::None,
                    dry_run: true,
                });
            }
            let staged = self.fetch_verify_stage_binary(target_version).await?;
            let approved_by = options
                .approved_by
                .clone()
                .or_else(|| Some("core_update_source".to_string()));
            let request = RestartRequest {
                reason: format!("core update evo@{target_version}"),
                target_binary: Some(staged.clone()),
                approved_by,
            };
            // Validate before spawning so a missing /
            // non-executable staged binary surfaces back to
            // the caller as ApplyFailed rather than
            // disappearing into a background task.
            self.restart_coordinator.validate(&request).map_err(|e| {
                UpdateError::ApplyFailed(format!(
                    "restart pre-validation failed: {e}"
                ))
            })?;
            // Spawn the restart so this async fn returns the
            // outcome before exec replaces the process.
            let coord = Arc::clone(&self.restart_coordinator);
            tokio::spawn(async move {
                // initiate returns Result<Infallible, _> —
                // success exec-replaces this process so the
                // Ok branch is unreachable; the Err branch
                // is the only arm we observe.
                let Err(e) = coord.initiate(request).await;
                tracing::error!(
                    error = %e,
                    "core update apply: graceful restart failed"
                );
            });
            Ok(UpdateOutcome {
                id: id.clone(),
                component: component.to_string(),
                applied_version: target_version.to_string(),
                restart_initiated: RestartLevel::Steward,
                dry_run: false,
            })
        })
    }
}

fn trim_trailing_slashes(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

fn strip_v_prefix(s: &str) -> &str {
    s.strip_prefix('v').unwrap_or(s)
}

fn sanitise_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn fetch_bounded(url: &str, cap: u64) -> Result<Vec<u8>, UpdateError> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(UpdateError::ApplyFailed(format!(
            "channel URL must use http(s):// (got {url:?})"
        )));
    }
    let response = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .get(url)
        .call()
        .map_err(|e| {
            UpdateError::SourceUnreachable(format!("fetch {url}: {e}"))
        })?;
    if !(200..300).contains(&response.status()) {
        return Err(UpdateError::SourceUnreachable(format!(
            "HTTP {} fetching {url}",
            response.status()
        )));
    }
    let mut body = Vec::with_capacity(64 * 1024);
    use std::io::Read;
    response
        .into_reader()
        .take(cap.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|e| {
            UpdateError::SourceUnreachable(format!(
                "reading body of {url}: {e}"
            ))
        })?;
    if body.len() as u64 > cap {
        return Err(UpdateError::ApplyFailed(format!(
            "{url} exceeds {cap} bytes"
        )));
    }
    Ok(body)
}

#[cfg(unix)]
fn write_executable(
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<(), UpdateError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(path)
        .map_err(|e| {
            UpdateError::ApplyFailed(format!(
                "opening staged path {}: {e}",
                path.display()
            ))
        })?;
    use std::io::Write;
    f.write_all(bytes).map_err(|e| {
        UpdateError::ApplyFailed(format!(
            "writing staged binary to {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn write_executable(
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<(), UpdateError> {
    std::fs::write(path, bytes).map_err(|e| {
        UpdateError::ApplyFailed(format!(
            "writing staged binary to {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_strict_semver() {
        assert!(CoreUpdateSource::is_newer("0.1.12", "0.1.13"));
        assert!(CoreUpdateSource::is_newer("0.1.12", "0.2.0"));
        assert!(CoreUpdateSource::is_newer("0.1.12", "v0.1.13"));
        assert!(!CoreUpdateSource::is_newer("0.1.13", "0.1.13"));
        assert!(!CoreUpdateSource::is_newer("0.1.13", "0.1.12"));
    }

    #[test]
    fn is_newer_refuses_unparseable() {
        assert!(!CoreUpdateSource::is_newer("not-a-version", "0.1.13"));
        assert!(!CoreUpdateSource::is_newer("0.1.12", "rolling"));
    }

    #[test]
    fn strip_v_prefix_handles_with_and_without() {
        assert_eq!(strip_v_prefix("v0.1.13"), "0.1.13");
        assert_eq!(strip_v_prefix("0.1.13"), "0.1.13");
        assert_eq!(strip_v_prefix("vNothing"), "Nothing");
    }

    #[test]
    fn trim_trailing_slashes_normalises() {
        assert_eq!(trim_trailing_slashes("https://x/y"), "https://x/y");
        assert_eq!(trim_trailing_slashes("https://x/y/"), "https://x/y");
        assert_eq!(trim_trailing_slashes("https://x/y///"), "https://x/y");
    }

    #[test]
    fn sanitise_filename_replaces_unsafe() {
        assert_eq!(sanitise_filename("0.1.13"), "0.1.13");
        assert_eq!(sanitise_filename("0.1.13-rc.1"), "0.1.13-rc.1");
        assert_eq!(sanitise_filename("../escape"), ".._escape");
        assert_eq!(sanitise_filename("a b c"), "a_b_c");
    }

    #[test]
    fn manifest_parses_real_shape() {
        // Real shape lifted from
        // foonerd/evo-core-artefacts/binaries/aarch64-unknown-linux-gnu/build-info.toml
        let toml_text = r#"
schema_version = 0
kind = "core-binaries"
evo_core_tag = "v0.1.12"
target = "aarch64-unknown-linux-gnu"
binaries = ["evo", "evo-plugin-tool"]
built_at = "2026-05-02T17:41:49Z"
publisher = "org.evoframework.core"

[[auxiliary]]
path = "dist/systemd/evo.service.example"
sha256 = "deadbeef"
"#;
        let m: CoreReleaseManifest = toml::from_str(toml_text).unwrap();
        assert_eq!(m.kind, "core-binaries");
        assert_eq!(m.evo_core_tag, "v0.1.12");
        assert_eq!(m.target, "aarch64-unknown-linux-gnu");
        assert_eq!(m.binaries, vec!["evo", "evo-plugin-tool"]);
    }

    #[test]
    fn fetch_bounded_refuses_non_http_scheme() {
        let r = fetch_bounded("file:///etc/passwd", 1024);
        assert!(matches!(r, Err(UpdateError::ApplyFailed(_))));
    }
}
