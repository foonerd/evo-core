// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Plugin-registry update source — the framework-default
//! `"plugins"` source.
//!
//! Walks every registered plugin registry's cached manifest,
//! compares each listing's best-version-for-host against the
//! locally-admitted plugin's version, and reports newer
//! versions as [`UpdateAvailable`] entries through the
//! [`UpdateSource`] trait. `apply_update` stages the
//! registry-supplied bundle URL to the plugin stage
//! directory; the existing four-path-admission watcher then
//! picks up the staged bundle and admits the new version
//! asynchronously.
//!
//! ## Design
//!
//! The source holds three substrate handles:
//!
//! - The [`crate::plugin_registry::PluginRegistryRuntime`]
//!   provides the registered registry list + cached
//!   per-registry [`crate::plugin_registry::RegistryManifest`]
//!   snapshots.
//! - The [`crate::router::PluginRouter`] provides the
//!   currently-admitted plugin set with their manifest-
//!   declared versions.
//! - The plugin stage directory path is where bundles land;
//!   the framework's watcher consumes from here.
//!
//! No [`crate::admission::AdmissionEngine`] handle is
//! needed: the source's apply path is "stage the bundle";
//! admission is the watcher's job. Decoupling keeps the
//! source easy to reason about + hot-swappable across vendor
//! distributions if a vendor wishes to ship its own
//! plugin-registry source instead.
//!
//! ## Current substrate scope
//!
//! Sub-primitive I1 of the three-channel update model. Out of
//! scope, named explicitly:
//!
//! - **Theme / UI-shell / widget-pack updates** — these
//!   plugin kinds ride the UI track ADR; the
//!   `plugin_registry` source treats every listing
//!   uniformly today, so when the kinds admit through the
//!   same registry path the source surfaces them without
//!   modification.
//! - **Severity classification + size estimate + changelog
//!   URL** — the registry manifest schema does not yet
//!   carry these fields. The source emits `Routine` /
//!   `None` defaults; adding the manifest fields rides a
//!   schema-bump iteration on the registry-driven half of
//!   the four-path-admission ADR.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use evo_plugin_sdk::update::{
    ApplyOptions, RestartLevel, SourceCapabilities, SourceId, UpdateAvailable,
    UpdateError, UpdateId, UpdateOutcome, UpdateSeverity, UpdateSource,
};

use crate::plugin_registry::{
    host_architecture, pick_version_for_arch, PluginRegistryRuntime,
    RegistryManifest, RegistryPluginListing, RegistryPluginVersion,
};
use crate::router::PluginRouter;

/// Maximum bundle size the source will stage. Mirrors the
/// existing `install_plugin_from_url` cap so registry-driven
/// applies do not exceed the operator-issued path's bound.
const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;

/// HTTP request timeout for bundle fetches. Generous to
/// tolerate slow registries; bounded so a hung registry does
/// not stall the source.
const FETCH_TIMEOUT_SECS: u64 = 120;

/// Source-id the framework expects for the registry-driven
/// plugin update source.
pub const SOURCE_ID: &str = "plugins";

/// Plugin-registry update source.
pub struct PluginRegistrySource {
    registry_runtime: Arc<PluginRegistryRuntime>,
    router: Arc<PluginRouter>,
    stage_dir: PathBuf,
}

impl std::fmt::Debug for PluginRegistrySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistrySource")
            .field("stage_dir", &self.stage_dir)
            .finish_non_exhaustive()
    }
}

impl PluginRegistrySource {
    /// Construct a source. The stage directory is the same
    /// `<plugin_data_root>/stage` directory the existing
    /// `install_plugin_from_url` wire op stages to; the
    /// admission watcher consumes from here. Caller ensures
    /// the directory exists (boot-time wiring already does
    /// this).
    pub fn new(
        registry_runtime: Arc<PluginRegistryRuntime>,
        router: Arc<PluginRouter>,
        stage_dir: PathBuf,
    ) -> Self {
        Self {
            registry_runtime,
            router,
            stage_dir,
        }
    }

    /// Look up the locally-admitted version of a plugin.
    /// Returns `None` when the plugin is unknown (the
    /// registry surfaced a plugin not currently admitted) or
    /// when the manifest has no version field.
    fn admitted_version(&self, plugin_name: &str) -> Option<String> {
        let entry = self.router.lookup_by_name(plugin_name)?;
        let manifest = entry.manifest.lock().ok()?;
        manifest.as_ref().map(|m| m.plugin.version.to_string())
    }

    /// Compare two semver versions; return `true` when the
    /// remote is strictly newer than local. Versions that
    /// fail to parse as semver are conservatively treated as
    /// "not newer" — the source declines to upgrade across
    /// non-semver version strings.
    fn is_newer(local: &str, remote: &str) -> bool {
        let l = match semver::Version::parse(local) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let r = match semver::Version::parse(remote) {
            Ok(v) => v,
            Err(_) => return false,
        };
        r > l
    }

    /// Walk every registered registry's cached manifest;
    /// compose the upgrade list. Pure read path — no I/O,
    /// no mutation. The caller's task pool drives this from
    /// `check_for_updates`.
    async fn collect_updates(&self) -> Vec<UpdateAvailable> {
        let arch = host_architecture();
        let mut out: Vec<UpdateAvailable> = Vec::new();
        for record in self.registry_runtime.list().await {
            let snapshot = self.registry_runtime.snapshot(&record.slug).await;
            let manifest: RegistryManifest = match snapshot {
                Some((_, Some(m))) => m,
                _ => continue,
            };
            for listing in &manifest.plugins {
                let Some(local) = self.admitted_version(&listing.name) else {
                    // Plugin in the registry isn't admitted
                    // locally — discovery, not an upgrade.
                    // Sub-primitive I1 surfaces upgrades of
                    // already-admitted plugins; first-time
                    // installs ride the operator-issued
                    // install path.
                    continue;
                };
                let Some(remote) = pick_version_for_arch(listing, arch) else {
                    continue;
                };
                if !Self::is_newer(&local, &remote.version) {
                    continue;
                }
                out.push(make_update_available(listing, remote, &local));
            }
        }
        out.sort_by(|a, b| a.component.cmp(&b.component));
        out
    }

    /// Locate one update by id within the registered
    /// registries. Returns the bundle URL + optional
    /// signature value. Used by `apply_update`.
    async fn locate_bundle(
        &self,
        update_id: &UpdateId,
    ) -> Result<(String, Option<String>), UpdateError> {
        // Update id format: "<plugin_name>@<version>".
        let raw = update_id.as_str();
        let (plugin_name, target_version) =
            raw.rsplit_once('@').ok_or_else(|| {
                UpdateError::UnknownUpdate(format!(
                    "malformed update id (expected plugin@version): {raw}"
                ))
            })?;
        for record in self.registry_runtime.list().await {
            let snapshot = self.registry_runtime.snapshot(&record.slug).await;
            let manifest = match snapshot {
                Some((_, Some(m))) => m,
                _ => continue,
            };
            let Some(listing) =
                manifest.plugins.iter().find(|p| p.name == plugin_name)
            else {
                continue;
            };
            let Some(version) = listing
                .versions
                .iter()
                .find(|v| v.version == target_version)
            else {
                continue;
            };
            return Ok((version.url.clone(), version.signature.clone()));
        }
        Err(UpdateError::UnknownUpdate(raw.to_string()))
    }

    /// Stage the bundle to the watch directory + write a
    /// sidecar signature pin where the watcher / admission
    /// engine can read it. The watcher then admits the bundle
    /// asynchronously per the four-path-admission flow.
    async fn stage_bundle(
        &self,
        url: &str,
        signature: Option<&str>,
        plugin_name: &str,
        target_version: &str,
    ) -> Result<PathBuf, UpdateError> {
        if !url.starts_with("https://") {
            return Err(UpdateError::ApplyFailed(format!(
                "bundle URL must use https:// (got {url:?})"
            )));
        }
        std::fs::create_dir_all(&self.stage_dir).map_err(|e| {
            UpdateError::ApplyFailed(format!(
                "creating stage dir {}: {e}",
                self.stage_dir.display()
            ))
        })?;
        let url = url.to_string();
        let stage_dir = self.stage_dir.clone();
        let plugin_name = plugin_name.to_string();
        let target_version = target_version.to_string();
        let signature = signature.map(|s| s.to_string());
        // Run the blocking ureq call on a dedicated blocking
        // thread so we don't stall the tokio runtime.
        let staged_path = tokio::task::spawn_blocking(move || {
            fetch_to_stage(
                &url,
                &stage_dir,
                &plugin_name,
                &target_version,
                signature.as_deref(),
            )
        })
        .await
        .map_err(|e| {
            UpdateError::Internal(format!("stage task join failed: {e}"))
        })??;
        Ok(staged_path)
    }
}

impl UpdateSource for PluginRegistrySource {
    fn source_id(&self) -> SourceId {
        SourceId::new(SOURCE_ID)
    }

    fn display_name(&self) -> String {
        "Plugin updates (registries)".to_string()
    }

    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            background_check: true,
            atomic_apply: true,
            requires_restart: RestartLevel::Plugin,
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
        Box::pin(async move { Ok(self.collect_updates().await) })
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
            let (plugin_name, target_version) =
                raw.rsplit_once('@').ok_or_else(|| {
                    UpdateError::UnknownUpdate(format!(
                        "malformed update id: {raw}"
                    ))
                })?;
            let (url, signature) = self.locate_bundle(id).await?;
            if options.dry_run {
                // Dry run: locate the bundle (verifies the
                // update exists in a registry's manifest)
                // but stop short of fetching it. The source
                // declares atomic_apply and the dry-run path
                // here is the natural "would this work"
                // question.
                return Ok(UpdateOutcome {
                    id: id.clone(),
                    component: plugin_name.to_string(),
                    applied_version: target_version.to_string(),
                    restart_initiated: RestartLevel::None,
                    dry_run: true,
                });
            }
            let _staged = self
                .stage_bundle(
                    &url,
                    signature.as_deref(),
                    plugin_name,
                    target_version,
                )
                .await?;
            Ok(UpdateOutcome {
                id: id.clone(),
                component: plugin_name.to_string(),
                applied_version: target_version.to_string(),
                // The watcher will admit the staged bundle
                // and that admission may restart the running
                // plugin (per its lifecycle.hot_reload
                // declaration). We surface Plugin as the
                // worst-case the source's capabilities
                // declared.
                restart_initiated: RestartLevel::Plugin,
                dry_run: false,
            })
        })
    }
}

/// Compose an [`UpdateAvailable`] from a registry listing +
/// remote version + locally-admitted version.
fn make_update_available(
    listing: &RegistryPluginListing,
    remote: &RegistryPluginVersion,
    local_version: &str,
) -> UpdateAvailable {
    UpdateAvailable {
        id: UpdateId::new(format!("{}@{}", listing.name, remote.version)),
        component: listing.name.clone(),
        current_version: local_version.to_string(),
        available_version: remote.version.clone(),
        // Registry manifest schema does not yet carry per-
        // version changelog URL; leave None until the
        // schema bump.
        changelog_url: None,
        // Registry manifest schema does not yet carry
        // severity classification; default to Routine.
        // Schema bump in a follow-on iteration.
        severity: UpdateSeverity::Routine,
        // Registry manifest schema does not yet carry size
        // estimates per version.
        size_bytes: None,
        requires_restart: RestartLevel::Plugin,
        // Registry manifest schema does not yet carry
        // per-version published-at timestamps; surface the
        // UNIX epoch placeholder until the schema carries
        // it.
        published_at: std::time::SystemTime::UNIX_EPOCH,
    }
}

/// Synchronous bundle fetcher — runs on a blocking thread.
fn fetch_to_stage(
    url: &str,
    stage_dir: &std::path::Path,
    plugin_name: &str,
    target_version: &str,
    signature: Option<&str>,
) -> Result<PathBuf, UpdateError> {
    let response = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .get(url)
        .call()
        .map_err(|e| {
            UpdateError::SourceUnreachable(format!("HTTPS fetch of {url}: {e}"))
        })?;
    if !(200..300).contains(&response.status()) {
        return Err(UpdateError::SourceUnreachable(format!(
            "HTTP {} fetching {url}",
            response.status()
        )));
    }
    let cap = MAX_BUNDLE_BYTES.saturating_add(1);
    let mut body = Vec::with_capacity(64 * 1024);
    use std::io::Read;
    response
        .into_reader()
        .take(cap)
        .read_to_end(&mut body)
        .map_err(|e| {
            UpdateError::SourceUnreachable(format!(
                "reading body of {url}: {e}"
            ))
        })?;
    if body.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(UpdateError::ApplyFailed(format!(
            "bundle from {url} exceeds {MAX_BUNDLE_BYTES} bytes"
        )));
    }
    // Pick a stable filename derived from plugin name +
    // version + a millisecond suffix for uniqueness.
    let extension = pick_archive_extension(url).unwrap_or(".tar.gz");
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let safe_name = sanitise_filename(plugin_name);
    let safe_version = sanitise_filename(target_version);
    let bundle_filename =
        format!("{safe_name}-{safe_version}-{timestamp_ms}{extension}");
    let bundle_path = stage_dir.join(&bundle_filename);
    std::fs::write(&bundle_path, &body).map_err(|e| {
        UpdateError::ApplyFailed(format!(
            "writing staged bundle to {}: {e}",
            bundle_path.display()
        ))
    })?;
    if let Some(sig) = signature {
        let sig_path = bundle_path.with_extension(format!(
            "{}.sig",
            extension.trim_start_matches('.')
        ));
        std::fs::write(&sig_path, sig).map_err(|e| {
            UpdateError::ApplyFailed(format!(
                "writing signature sidecar to {}: {e}",
                sig_path.display()
            ))
        })?;
    }
    Ok(bundle_path)
}

fn pick_archive_extension(url: &str) -> Option<&'static str> {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(".tar.gz")
    } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
        Some(".tar.xz")
    } else if lower.ends_with(".zip") {
        Some(".zip")
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_returns_true_for_strictly_higher_semver() {
        assert!(PluginRegistrySource::is_newer("1.0.0", "1.0.1"));
        assert!(PluginRegistrySource::is_newer("1.0.0", "1.1.0"));
        assert!(PluginRegistrySource::is_newer("1.0.0", "2.0.0"));
    }

    #[test]
    fn is_newer_returns_false_for_equal_or_lower() {
        assert!(!PluginRegistrySource::is_newer("1.0.1", "1.0.0"));
        assert!(!PluginRegistrySource::is_newer("1.0.0", "1.0.0"));
        assert!(!PluginRegistrySource::is_newer("2.0.0", "1.9.9"));
    }

    #[test]
    fn is_newer_returns_false_for_unparseable_version_strings() {
        // Refuse to upgrade across non-semver versions; the
        // source is conservative.
        assert!(!PluginRegistrySource::is_newer("not-a-version", "1.0.0"));
        assert!(!PluginRegistrySource::is_newer("1.0.0", "not-a-version"));
    }

    #[test]
    fn pick_archive_extension_recognises_canonical_formats() {
        assert_eq!(
            pick_archive_extension("https://example/foo.tar.gz"),
            Some(".tar.gz")
        );
        assert_eq!(
            pick_archive_extension("https://example/foo.tar.xz"),
            Some(".tar.xz")
        );
        assert_eq!(
            pick_archive_extension("https://example/foo.zip"),
            Some(".zip")
        );
        assert_eq!(
            pick_archive_extension("https://example/foo.tgz"),
            Some(".tar.gz")
        );
        assert_eq!(pick_archive_extension("https://example/foo"), None);
    }

    #[test]
    fn sanitise_filename_strips_unsafe_characters() {
        assert_eq!(
            sanitise_filename("com.example/plugin"),
            "com.example_plugin"
        );
        assert_eq!(
            sanitise_filename("com.example.plugin"),
            "com.example.plugin"
        );
        assert_eq!(sanitise_filename("a b c"), "a_b_c");
        assert_eq!(sanitise_filename("../escape"), ".._escape");
    }

    #[test]
    fn make_update_available_format_is_plugin_at_version() {
        let listing = RegistryPluginListing {
            name: "com.example.plugin".into(),
            versions: vec![],
            description: None,
            description_key: None,
            author: None,
            screenshots: vec![],
            licence: None,
            architectures: vec![],
            dependencies: vec![],
            plugin_kind: "functional".into(),
            trust_class: "vendor".into(),
        };
        let remote = RegistryPluginVersion {
            version: "1.3.0".into(),
            url: "https://example/plugin.tar.gz".into(),
            signature: None,
            sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                .into(),
            min_evo_version: None,
        };
        let upd = make_update_available(&listing, &remote, "1.2.0");
        assert_eq!(upd.id.as_str(), "com.example.plugin@1.3.0");
        assert_eq!(upd.component, "com.example.plugin");
        assert_eq!(upd.current_version, "1.2.0");
        assert_eq!(upd.available_version, "1.3.0");
        assert_eq!(upd.severity, UpdateSeverity::Routine);
        assert_eq!(upd.requires_restart, RestartLevel::Plugin);
    }
}
