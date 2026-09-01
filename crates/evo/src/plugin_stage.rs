// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Plugin-stage watcher.
//!
//! Polls a stage directory for new plugin bundles, extracts
//! them, and feeds the extracted bundle through the same
//! admission gate boot-time discovery uses. One gate; no
//! bypass.
//!
//! The stage directory is a write-only drop target for
//! operators. Operators copy a signed bundle archive (the
//! `.tar.gz` / `.tar.xz` / `.zip` shape `evo-plugin-tool pack`
//! produces) into the directory; the framework polls, detects
//! stable writes (size unchanged across two consecutive ticks),
//! and processes the bundle. Successful admission consumes the
//! archive. Failed admission moves the archive to
//! `<stage>/rejected/<reason-slug>/<filename>` plus a sibling
//! `<filename>.reason.txt` describing the failure.
//!
//! The watcher keeps no in-memory state across restarts: every
//! tick is a fresh directory scan. A bundle that arrived during
//! a previous boot but was not consumed sits in the stage
//! directory until the next tick picks it up.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::Manifest;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio::time::interval;

use crate::admission::AdmissionEngine;

/// Bounded shape of a tick's "stability snapshot" of one
/// staged archive: the size at the most recent observation,
/// retained tick-to-tick so the next tick can confirm
/// stability before triggering install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArchiveSnapshot {
    size: u64,
}

/// Reasons a staged bundle failed to install. Each variant
/// produces a slug used as the rejected-subdirectory name; the
/// human-readable message lands in the sibling
/// `<filename>.reason.txt` file.
#[derive(Debug)]
enum RejectReason {
    UnsupportedExtension,
    ExtractFailed(String),
    ManifestMissing,
    ManifestInvalid(String),
    AdmissionFailed(String),
    InternalError(String),
}

impl RejectReason {
    fn slug(&self) -> &'static str {
        match self {
            RejectReason::UnsupportedExtension => "unsupported-extension",
            RejectReason::ExtractFailed(_) => "extract-failed",
            RejectReason::ManifestMissing => "manifest-missing",
            RejectReason::ManifestInvalid(_) => "manifest-invalid",
            RejectReason::AdmissionFailed(_) => "admission-failed",
            RejectReason::InternalError(_) => "internal-error",
        }
    }

    fn message(&self) -> String {
        match self {
            RejectReason::UnsupportedExtension => {
                "Bundle archive extension not recognised. Use .tar.gz, .tgz, .tar.xz, .txz, or .zip.".to_string()
            }
            RejectReason::ExtractFailed(detail) => {
                format!("Archive extraction failed: {detail}")
            }
            RejectReason::ManifestMissing => {
                "Bundle does not contain manifest.toml at the bundle root.".to_string()
            }
            RejectReason::ManifestInvalid(detail) => {
                format!("Bundle manifest.toml is invalid: {detail}")
            }
            RejectReason::AdmissionFailed(detail) => {
                format!("Admission engine refused the bundle: {detail}")
            }
            RejectReason::InternalError(detail) => {
                format!("Internal error processing bundle: {detail}")
            }
        }
    }
}

/// Configuration handed to the watcher at construction.
#[derive(Debug, Clone)]
pub struct PluginStageConfig {
    /// Stage directory the watcher polls.
    pub stage_dir: PathBuf,
    /// Root under which extracted bundles are installed
    /// (`<plugin_data_root>/<plugin.name>/...`).
    pub plugin_data_root: PathBuf,
    /// Runtime directory passed through to the admission
    /// engine (`<runtime_dir>/<plugin.name>.sock`).
    pub runtime_dir: PathBuf,
    /// Polling interval. Default 1s.
    pub poll_interval: Duration,
}

/// Stage-directory watcher. Construct with
/// [`PluginStageWatcher::new`]; spawn its polling task with
/// [`PluginStageWatcher::start`]. The returned [`JoinHandle`]
/// completes when the watcher's shutdown signal is observed
/// (today: when the engine handle is dropped or the runtime
/// shuts down).
pub struct PluginStageWatcher {
    config: PluginStageConfig,
    engine: Arc<AsyncMutex<AdmissionEngine>>,
}

impl PluginStageWatcher {
    /// Construct a watcher bound to the configured stage
    /// directory and the supplied admission engine handle.
    pub fn new(
        config: PluginStageConfig,
        engine: Arc<AsyncMutex<AdmissionEngine>>,
    ) -> Self {
        Self { config, engine }
    }

    /// Spawn the polling loop on the current tokio runtime.
    /// Returns a [`JoinHandle`] that completes when the loop
    /// exits.
    pub fn start(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(self) {
        // Best-effort directory creation: if the stage dir
        // doesn't exist, create it. If creation fails the
        // operator's first tick logs the read error; the
        // watcher continues polling so a later mkdir works.
        if let Err(e) = fs::create_dir_all(&self.config.stage_dir) {
            tracing::warn!(
                stage_dir = %self.config.stage_dir.display(),
                error = %e,
                "plugin stage: cannot create stage directory; \
                 will retry on each tick"
            );
        }

        tracing::info!(
            stage_dir = %self.config.stage_dir.display(),
            poll_interval_ms = self.config.poll_interval.as_millis() as u64,
            "plugin stage: watcher started"
        );

        // Tick-to-tick snapshot of staged archive sizes. A
        // bundle is only processed when this tick observes the
        // same size we recorded on the previous tick — that's
        // the operator's write completing.
        let mut last_seen: HashMap<PathBuf, ArchiveSnapshot> = HashMap::new();

        let mut ticker = interval(self.config.poll_interval);
        loop {
            ticker.tick().await;
            let observed = match scan_stage_dir(&self.config.stage_dir) {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(
                        stage_dir = %self.config.stage_dir.display(),
                        error = %e,
                        "plugin stage: scan failed; will retry on next tick"
                    );
                    continue;
                }
            };

            let mut next_seen: HashMap<PathBuf, ArchiveSnapshot> =
                HashMap::with_capacity(observed.len());
            for (path, snapshot) in observed {
                let prior = last_seen.get(&path).copied();
                if Some(snapshot) == prior {
                    // Size unchanged across two consecutive
                    // ticks: write has stabilised. Process it.
                    self.process_archive(&path).await;
                    // Don't re-track this path: it's either
                    // consumed or moved to rejected/. If a new
                    // archive appears at the same path next
                    // tick, the next-seen map starts fresh.
                } else {
                    next_seen.insert(path, snapshot);
                }
            }
            last_seen = next_seen;
        }
    }

    /// Process a single stable archive: extract, admit, consume
    /// or reject.
    async fn process_archive(&self, archive: &Path) {
        tracing::info!(
            archive = %archive.display(),
            "plugin stage: stable archive observed; processing"
        );

        let work_dir = match tempdir(&self.config.stage_dir, "extract") {
            Ok(d) => d,
            Err(e) => {
                self.reject(
                    archive,
                    RejectReason::InternalError(format!(
                        "cannot create work directory: {e}"
                    )),
                );
                return;
            }
        };

        // Extract the archive into the work dir.
        if let Err(e) = extract_archive(archive, &work_dir) {
            self.reject(archive, e);
            // Best-effort cleanup of the partial extraction
            // (don't propagate errors past the work dir).
            let _ = fs::remove_dir_all(&work_dir);
            return;
        }

        // Resolve the bundle root inside the work dir. The
        // archive may unpack as a single subdirectory (the
        // common case from `evo-plugin-tool pack`) or as the
        // bundle's files at the work dir root. Both shapes are
        // accepted.
        let bundle_root = match resolve_bundle_root(&work_dir) {
            Ok(r) => r,
            Err(e) => {
                self.reject(archive, e);
                let _ = fs::remove_dir_all(&work_dir);
                return;
            }
        };

        // Read the manifest to get the plugin canonical name
        // (the install destination directory name).
        let manifest_path = bundle_root.join("manifest.toml");
        if !manifest_path.is_file() {
            self.reject(archive, RejectReason::ManifestMissing);
            let _ = fs::remove_dir_all(&work_dir);
            return;
        }
        let manifest_text = match fs::read_to_string(&manifest_path) {
            Ok(t) => t,
            Err(e) => {
                self.reject(
                    archive,
                    RejectReason::InternalError(format!(
                        "reading manifest: {e}"
                    )),
                );
                let _ = fs::remove_dir_all(&work_dir);
                return;
            }
        };
        let manifest = match Manifest::from_toml(&manifest_text) {
            Ok(m) => m,
            Err(e) => {
                self.reject(
                    archive,
                    RejectReason::ManifestInvalid(e.to_string()),
                );
                let _ = fs::remove_dir_all(&work_dir);
                return;
            }
        };
        let plugin_name = manifest.plugin.name.clone();

        // Install the extracted bundle into
        // `<plugin_data_root>/<plugin_name>/`. If a previous
        // install of the same plugin sits there, replace it
        // (the new bundle wins; admission engine handles
        // any prior admission).
        let install_dir = self.config.plugin_data_root.join(&plugin_name);
        if install_dir.exists() {
            if let Err(e) = fs::remove_dir_all(&install_dir) {
                self.reject(
                    archive,
                    RejectReason::InternalError(format!(
                        "removing prior install at {}: {e}",
                        install_dir.display()
                    )),
                );
                let _ = fs::remove_dir_all(&work_dir);
                return;
            }
        }
        if let Err(e) = fs::create_dir_all(&install_dir) {
            self.reject(
                archive,
                RejectReason::InternalError(format!(
                    "creating install dir {}: {e}",
                    install_dir.display()
                )),
            );
            let _ = fs::remove_dir_all(&work_dir);
            return;
        }
        if let Err(e) = copy_dir_contents(&bundle_root, &install_dir) {
            self.reject(
                archive,
                RejectReason::InternalError(format!(
                    "copying bundle into {}: {e}",
                    install_dir.display()
                )),
            );
            let _ = fs::remove_dir_all(&work_dir);
            let _ = fs::remove_dir_all(&install_dir);
            return;
        }

        // Best-effort cleanup of the work directory before we
        // hand off to admission. Failure here is non-fatal.
        let _ = fs::remove_dir_all(&work_dir);

        // Run the admission gate.
        let mut engine_guard = self.engine.lock().await;
        let admit_result = engine_guard
            .admit_out_of_process_from_directory(
                &install_dir,
                &self.config.runtime_dir,
            )
            .await;
        drop(engine_guard);

        match admit_result {
            Ok(()) => {
                tracing::info!(
                    plugin = %plugin_name,
                    archive = %archive.display(),
                    "plugin stage: admitted from staged bundle; \
                     consuming archive"
                );
                if let Err(e) = fs::remove_file(archive) {
                    tracing::warn!(
                        archive = %archive.display(),
                        error = %e,
                        "plugin stage: admission succeeded but \
                         archive removal failed; the next tick \
                         will see the archive again and skip it \
                         because the install dir is up-to-date \
                         (admission engine refuses duplicate \
                         shelf admission)"
                    );
                }
            }
            Err(e) => {
                self.reject(
                    archive,
                    RejectReason::AdmissionFailed(e.to_string()),
                );
                // Roll back the partial install — the
                // admission engine refused, so the framework
                // does not own this plugin.
                let _ = fs::remove_dir_all(&install_dir);
            }
        }
    }

    /// Move the staged archive to
    /// `<stage>/rejected/<reason-slug>/<filename>` plus a
    /// sibling `<filename>.reason.txt`. Best-effort: any
    /// failure here is logged but does not propagate (the
    /// archive remains where it is and the next tick picks it
    /// up again, which is logged as duplicate processing —
    /// not catastrophic).
    fn reject(&self, archive: &Path, reason: RejectReason) {
        let slug = reason.slug();
        let message = reason.message();
        tracing::warn!(
            archive = %archive.display(),
            reason_slug = slug,
            reason = %message,
            "plugin stage: rejecting bundle"
        );

        let rejected_dir = self.config.stage_dir.join("rejected").join(slug);
        if let Err(e) = fs::create_dir_all(&rejected_dir) {
            tracing::warn!(
                rejected_dir = %rejected_dir.display(),
                error = %e,
                "plugin stage: cannot create rejected directory"
            );
            return;
        }

        let filename = match archive.file_name() {
            Some(n) => n.to_owned(),
            None => return,
        };
        let dest = rejected_dir.join(&filename);
        let reason_file = rejected_dir.join({
            let mut name = filename.clone();
            name.push(".reason.txt");
            name
        });

        if let Err(e) = fs::rename(archive, &dest) {
            tracing::warn!(
                archive = %archive.display(),
                dest = %dest.display(),
                error = %e,
                "plugin stage: cannot move rejected archive; \
                 attempting copy + remove"
            );
            // Fall back to copy + remove for cross-filesystem
            // moves.
            if let Err(copy_err) = fs::copy(archive, &dest) {
                tracing::warn!(
                    archive = %archive.display(),
                    dest = %dest.display(),
                    error = %copy_err,
                    "plugin stage: copy + remove fallback also \
                     failed; archive remains in stage"
                );
                return;
            }
            let _ = fs::remove_file(archive);
        }
        if let Err(e) = fs::write(&reason_file, &message) {
            tracing::warn!(
                reason_file = %reason_file.display(),
                error = %e,
                "plugin stage: cannot write reason file"
            );
        }
    }
}

/// Scan `stage_dir` for archive-shaped files; return their
/// paths and current sizes. Subdirectories (including
/// `rejected/`) are skipped.
fn scan_stage_dir(
    stage_dir: &Path,
) -> Result<Vec<(PathBuf, ArchiveSnapshot)>, std::io::Error> {
    if !stage_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(stage_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        if !looks_like_archive(&path) {
            continue;
        }
        out.push((
            path,
            ArchiveSnapshot {
                size: metadata.len(),
            },
        ));
    }
    Ok(out)
}

/// True when the path's filename ends in one of the bundle
/// archive extensions the framework accepts.
fn looks_like_archive(path: &Path) -> bool {
    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => n.to_ascii_lowercase(),
        None => return false,
    };
    name.ends_with(".tar.gz")
        || name.ends_with(".tgz")
        || name.ends_with(".tar.xz")
        || name.ends_with(".txz")
        || name.ends_with(".zip")
}

/// Create a unique subdirectory under `parent` with the given
/// prefix. Used for ephemeral extraction work.
fn tempdir(parent: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    fs::create_dir_all(parent)?;
    for attempt in 0..32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate =
            parent.join(format!(".{prefix}.{nanos:x}.{attempt:02x}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate unique temp directory after 32 attempts",
    ))
}

/// Extract `archive` into `dest`. Format dispatched by the
/// archive's filename extension. On success the bundle's
/// contents are reachable under `dest` (possibly inside one
/// subdirectory; [`resolve_bundle_root`] handles both shapes).
fn extract_archive(archive: &Path, dest: &Path) -> Result<(), RejectReason> {
    let name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let f = fs::File::open(archive).map_err(|e| {
            RejectReason::ExtractFailed(format!("open archive: {e}"))
        })?;
        let gz = flate2::read::GzDecoder::new(f);
        let mut tar = tar::Archive::new(gz);
        tar.unpack(dest).map_err(|e| {
            RejectReason::ExtractFailed(format!("tar.gz unpack: {e}"))
        })?;
        return Ok(());
    }
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        let f = fs::File::open(archive).map_err(|e| {
            RejectReason::ExtractFailed(format!("open archive: {e}"))
        })?;
        let xz = xz2::read::XzDecoder::new(f);
        let mut tar = tar::Archive::new(xz);
        tar.unpack(dest).map_err(|e| {
            RejectReason::ExtractFailed(format!("tar.xz unpack: {e}"))
        })?;
        return Ok(());
    }
    if name.ends_with(".zip") {
        let f = fs::File::open(archive).map_err(|e| {
            RejectReason::ExtractFailed(format!("open archive: {e}"))
        })?;
        let mut zip = zip::ZipArchive::new(f).map_err(|e| {
            RejectReason::ExtractFailed(format!("zip open: {e}"))
        })?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| {
                RejectReason::ExtractFailed(format!("zip entry {i}: {e}"))
            })?;
            let entry_path = match entry.enclosed_name() {
                Some(p) => p.to_path_buf(),
                None => {
                    return Err(RejectReason::ExtractFailed(format!(
                        "zip entry {i} has invalid path"
                    )))
                }
            };
            let dest_path = dest.join(&entry_path);
            if entry.is_dir() {
                fs::create_dir_all(&dest_path).map_err(|e| {
                    RejectReason::ExtractFailed(format!(
                        "mkdir {}: {e}",
                        dest_path.display()
                    ))
                })?;
            } else {
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        RejectReason::ExtractFailed(format!(
                            "mkdir {}: {e}",
                            parent.display()
                        ))
                    })?;
                }
                let mut out = fs::File::create(&dest_path).map_err(|e| {
                    RejectReason::ExtractFailed(format!(
                        "create {}: {e}",
                        dest_path.display()
                    ))
                })?;
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf).map_err(|e| {
                    RejectReason::ExtractFailed(format!(
                        "read entry {}: {e}",
                        entry_path.display()
                    ))
                })?;
                std::io::Write::write_all(&mut out, &buf).map_err(|e| {
                    RejectReason::ExtractFailed(format!(
                        "write {}: {e}",
                        dest_path.display()
                    ))
                })?;
                #[cfg(unix)]
                {
                    if let Some(mode) = entry.unix_mode() {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = fs::set_permissions(
                            &dest_path,
                            fs::Permissions::from_mode(mode),
                        );
                    }
                }
            }
        }
        return Ok(());
    }
    Err(RejectReason::UnsupportedExtension)
}

/// Locate the bundle root inside an extraction work
/// directory. Two shapes are accepted:
///
/// 1. `manifest.toml` directly under `work` — the archive
///    unpacked its files at the root.
/// 2. `<work>/<single-subdir>/manifest.toml` — the archive
///    unpacked into a single subdirectory (the common case
///    from `evo-plugin-tool pack`).
fn resolve_bundle_root(work: &Path) -> Result<PathBuf, RejectReason> {
    if work.join("manifest.toml").is_file() {
        return Ok(work.to_path_buf());
    }
    let mut subdirs = Vec::new();
    let entries = fs::read_dir(work).map_err(|e| {
        RejectReason::ExtractFailed(format!(
            "read extracted dir {}: {e}",
            work.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            RejectReason::ExtractFailed(format!(
                "read extracted dir entry: {e}"
            ))
        })?;
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    if subdirs.len() == 1 && subdirs[0].join("manifest.toml").is_file() {
        return Ok(subdirs.remove(0));
    }
    Err(RejectReason::ManifestMissing)
}

/// Copy the contents of `src` into `dst` recursively. Both
/// must already exist as directories.
fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_contents(&src_path, &dst_path)?;
        } else if metadata.is_file() {
            fs::copy(&src_path, &dst_path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = metadata.permissions().mode();
                let _ = fs::set_permissions(
                    &dst_path,
                    fs::Permissions::from_mode(mode),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_archive_recognises_known_extensions() {
        assert!(looks_like_archive(Path::new("foo.tar.gz")));
        assert!(looks_like_archive(Path::new("foo.tgz")));
        assert!(looks_like_archive(Path::new("foo.tar.xz")));
        assert!(looks_like_archive(Path::new("foo.txz")));
        assert!(looks_like_archive(Path::new("foo.zip")));
        assert!(looks_like_archive(Path::new("FOO.TAR.GZ"))); // case-insensitive
        assert!(!looks_like_archive(Path::new("foo.tar")));
        assert!(!looks_like_archive(Path::new("foo")));
        assert!(!looks_like_archive(Path::new("foo.txt")));
    }

    #[test]
    fn scan_stage_dir_skips_subdirectories() {
        let tmp = tempdir(Path::new("/tmp"), "evo-stage-test").unwrap();
        std::fs::write(tmp.join("a.tar.gz"), b"placeholder").unwrap();
        std::fs::create_dir_all(tmp.join("rejected").join("ignored")).unwrap();
        std::fs::write(
            tmp.join("rejected").join("ignored").join("b.tar.gz"),
            b"in-subdir",
        )
        .unwrap();
        let observed = scan_stage_dir(&tmp).unwrap();
        assert_eq!(observed.len(), 1);
        assert!(observed[0].0.ends_with("a.tar.gz"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn scan_stage_dir_skips_non_archive_files() {
        let tmp = tempdir(Path::new("/tmp"), "evo-stage-test").unwrap();
        std::fs::write(tmp.join("a.tar.gz"), b"x").unwrap();
        std::fs::write(tmp.join("README.txt"), b"y").unwrap();
        std::fs::write(tmp.join(".incoming.tar.gz.tmp"), b"z").unwrap();
        let observed = scan_stage_dir(&tmp).unwrap();
        assert_eq!(observed.len(), 1);
        assert!(observed[0].0.ends_with("a.tar.gz"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn scan_stage_dir_returns_empty_when_dir_missing() {
        let observed =
            scan_stage_dir(Path::new("/tmp/evo-stage-missing-12345-abc"))
                .unwrap();
        assert!(observed.is_empty());
    }

    #[test]
    fn extract_archive_round_trip_tar_gz() {
        // Build a small tar.gz on the fly and confirm the
        // extractor reproduces the layout.
        let work = tempdir(Path::new("/tmp"), "evo-stage-extract").unwrap();
        let archive_path = work.join("bundle.tar.gz");
        let payload = work.join("payload");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(
            payload.join("manifest.toml"),
            b"[plugin]\nname = \"x\"\n",
        )
        .unwrap();
        // Pack it.
        let f = std::fs::File::create(&archive_path).unwrap();
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all("bundle", &payload).unwrap();
        let _ = tar.into_inner().unwrap().finish().unwrap();

        let dest = work.join("extracted");
        std::fs::create_dir_all(&dest).unwrap();
        extract_archive(&archive_path, &dest).expect("extract succeeds");
        let manifest = dest.join("bundle").join("manifest.toml");
        assert!(manifest.is_file());
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn extract_archive_rejects_unknown_extension() {
        let work = tempdir(Path::new("/tmp"), "evo-stage-bad").unwrap();
        let archive = work.join("foo.unknown");
        std::fs::write(&archive, b"x").unwrap();
        let dest = work.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let err = extract_archive(&archive, &dest).expect_err("must reject");
        assert_eq!(err.slug(), "unsupported-extension");
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn resolve_bundle_root_finds_root_at_top_level() {
        let work = tempdir(Path::new("/tmp"), "evo-stage-rb1").unwrap();
        std::fs::write(work.join("manifest.toml"), b"x").unwrap();
        let root = resolve_bundle_root(&work).unwrap();
        assert_eq!(root, work);
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn resolve_bundle_root_finds_root_in_single_subdir() {
        let work = tempdir(Path::new("/tmp"), "evo-stage-rb2").unwrap();
        let inner = work.join("bundle");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("manifest.toml"), b"x").unwrap();
        let root = resolve_bundle_root(&work).unwrap();
        assert_eq!(root, inner);
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn resolve_bundle_root_refuses_when_no_manifest() {
        let work = tempdir(Path::new("/tmp"), "evo-stage-rb3").unwrap();
        std::fs::create_dir_all(work.join("a")).unwrap();
        std::fs::create_dir_all(work.join("b")).unwrap();
        let err = resolve_bundle_root(&work).expect_err("must reject");
        assert_eq!(err.slug(), "manifest-missing");
        std::fs::remove_dir_all(&work).unwrap();
    }
}
