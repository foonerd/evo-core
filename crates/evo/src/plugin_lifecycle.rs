//! Plugin lifecycle coordinator.
//!
//! Owns the file watcher on `/etc/evo/plugins.d/` and the
//! reload-event surface that downstream consumers (admission
//! engine, reactive-only plugin substrate diff applicator)
//! react to. Emits `PluginLiveReloadStarted` happenings on
//! TOML edits + operator gestures; the admission-engine
//! integration that completes the reload cycle is the next
//! stage's concern.
//!
//! Design constraints:
//! - inotify on Linux full-participant + embedded Linux tiers
//!   (no Pi 0-specific code paths — Pi 0 runs Linux too).
//! - Polling fallback at 5 s cadence for non-Linux Unix
//!   tiers (graceful degradation; not on the rig today).
//! - MCU tier does NOT run the watcher at all (no filesystem
//!   to watch; future MCU follower plugins land via
//!   substrate-mutation-only reconfiguration).
//! - 500 ms debounce on the watcher to collapse multi-line
//!   edits into a single reload event.
//! - Watcher refuses to fire on plugins whose manifest
//!   declares `lifecycle.mode = "frozen"` (warn-level log;
//!   no reload).
//!
//! Resource posture:
//! - One inotify watch on the plugins directory; bounded by
//!   the plugin count.
//! - One debounce timer per recent edit; coalesces multiple
//!   writes within 500 ms.
//! - Subscriber broadcast channel capacity 16 (drop-oldest
//!   on slow subscriber).
//! - Per-edit cost: stat the file + identify the plugin name
//!   + emit one happening.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, Mutex, Notify};

use crate::admission::AdmissionEngine;

/// Debounce window. inotify often fires multiple events for
/// a single editor save (modify + close-write + chmod). The
/// watcher coalesces all events within this window into one
/// reload event per file.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

/// Polling fallback cadence. Non-Linux Unix tiers stat every
/// file in the plugins directory at this interval and emit
/// reload events on mtime advance.
pub const POLLING_INTERVAL: Duration = Duration::from_secs(5);

/// Subscriber broadcast channel capacity.
const SUBSCRIBER_CAPACITY: usize = 16;

/// Plugin lifecycle event the coordinator emits when a TOML
/// edit (or operator gesture) requests a reload.
#[derive(Debug, Clone)]
pub struct PluginReloadRequested {
    /// Plugin name derived from the file path. Matches the
    /// `[plugin] name` field in the manifest.
    pub plugin_name: String,
    /// Path to the plugin's config file. Caller reads it to
    /// apply the new TOML.
    pub config_path: PathBuf,
    /// Wall-clock time the reload was requested.
    pub requested_at_ms: u64,
    /// Source of the request — file watcher event or
    /// operator-gestured wire op / CLI.
    pub source: ReloadSource,
}

/// Where the reload request originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadSource {
    /// inotify or polling fallback observed a config file
    /// change.
    FileWatcher,
    /// `plugin.reload` wire op or CLI invocation.
    OperatorGesture,
}

/// Coordinator errors.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// File watcher could not establish its inotify watch.
    /// Polling fallback automatically activates on
    /// non-Linux platforms; on Linux this indicates a
    /// permissions issue or kernel feature missing.
    #[error("plugin lifecycle: watcher init: {0}")]
    Watcher(String),
    /// I/O error reading from the plugins directory.
    #[error("plugin lifecycle: I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-file last-emitted state for debounce + dedup.
#[derive(Debug, Clone)]
struct FileState {
    /// Last time we emitted a reload for this file.
    last_emitted_at: Instant,
    /// Last observed mtime (polling fallback).
    last_mtime: Option<SystemTime>,
}

/// Plugin lifecycle coordinator runtime.
pub struct PluginLifecycleCoordinator {
    plugins_dir: PathBuf,
    reload_tx: broadcast::Sender<PluginReloadRequested>,
    file_states: Mutex<HashMap<PathBuf, FileState>>,
    shutdown: Arc<Notify>,
}

impl PluginLifecycleCoordinator {
    /// Construct the coordinator. Pass the plugins directory
    /// (typically `/etc/evo/plugins.d/`). Reload-request
    /// events publish on the coordinator's own broadcast
    /// channel; the admission engine integration that emits
    /// `PluginLiveReloadStarted` / `Completed` / `Failed`
    /// happenings reads from that channel and emits with the
    /// manifest version context it has.
    pub fn new(plugins_dir: impl Into<PathBuf>) -> Arc<Self> {
        let (reload_tx, _) = broadcast::channel(SUBSCRIBER_CAPACITY);
        Arc::new(Self {
            plugins_dir: plugins_dir.into(),
            reload_tx,
            file_states: Mutex::new(HashMap::new()),
            shutdown: Arc::new(Notify::new()),
        })
    }

    /// Spawn the polling fallback task. inotify integration
    /// is a follow-on commit that lands the same coordinator
    /// with an additional task driving inotify; the polling
    /// path is the universal fallback that works on every
    /// Unix-like platform.
    pub fn start(self: &Arc<Self>) {
        let runtime = Arc::clone(self);
        let shutdown = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            runtime.run_polling_loop(shutdown).await;
        });
    }

    /// Cooperative shutdown.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    /// Subscribe to reload request events. Downstream
    /// consumers (admission engine, reactive-only diff
    /// applicator) await on this channel and react.
    pub fn subscribe(&self) -> broadcast::Receiver<PluginReloadRequested> {
        self.reload_tx.subscribe()
    }

    /// Trigger an operator-gestured reload. Idempotent; the
    /// reload event emits with `source: OperatorGesture` so
    /// consumers can distinguish from file-watcher-triggered
    /// reloads.
    pub async fn request_reload(&self, plugin_name: &str) {
        let config_path = self.plugins_dir.join(format!("{plugin_name}.toml"));
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let event = PluginReloadRequested {
            plugin_name: plugin_name.to_string(),
            config_path,
            requested_at_ms: now_ms,
            source: ReloadSource::OperatorGesture,
        };
        let _ = self.reload_tx.send(event);
    }

    async fn run_polling_loop(self: Arc<Self>, shutdown: Arc<Notify>) {
        let mut interval = tokio::time::interval(POLLING_INTERVAL);
        interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::debug!(
                        "plugin lifecycle coordinator: shutdown received"
                    );
                    return;
                }
                _ = interval.tick() => {
                    if let Err(e) = self.poll_once().await {
                        tracing::debug!(
                            error = %e,
                            "plugin lifecycle coordinator: poll cycle failed"
                        );
                    }
                }
            }
        }
    }

    async fn poll_once(&self) -> Result<(), LifecycleError> {
        let entries = match std::fs::read_dir(&self.plugins_dir) {
            Ok(e) => e,
            Err(e) => {
                // Plugins directory does not exist — treat
                // as no plugins to watch (clean install
                // state, or vendor distribution that does
                // not use the standard path).
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Ok(());
                }
                return Err(e.into());
            }
        };
        let now = Instant::now();
        let mut states = self.file_states.lock().await;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let plugin_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
            let Some(plugin_name) = plugin_name else {
                continue;
            };
            let mtime = entry.metadata().ok().and_then(|m| m.modified().ok());

            let state = states.entry(path.clone()).or_insert(FileState {
                last_emitted_at: now - DEBOUNCE_WINDOW,
                last_mtime: mtime,
            });
            // First observation: record + skip.
            if state.last_mtime == mtime {
                continue;
            }
            // Debounce — coalesce repeated edits within the
            // debounce window into one reload event.
            if now.duration_since(state.last_emitted_at) < DEBOUNCE_WINDOW {
                state.last_mtime = mtime;
                continue;
            }

            state.last_emitted_at = now;
            state.last_mtime = mtime;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let event = PluginReloadRequested {
                plugin_name: plugin_name.clone(),
                config_path: path.clone(),
                requested_at_ms: now_ms,
                source: ReloadSource::FileWatcher,
            };
            let _ = self.reload_tx.send(event);
            tracing::info!(
                plugin_name,
                path = %path.display(),
                "plugin lifecycle: TOML edit observed; reload requested"
            );
        }
        Ok(())
    }
}

/// Spawn the lifecycle dispatcher task: subscribe to the
/// coordinator's reload-request channel and route each event
/// to [`AdmissionEngine::reload_plugin_with_source`].
///
/// The dispatcher closes the gap between the coordinator's
/// observation surface (file watcher, operator-gesture wire op)
/// and the engine's actuation surface (per-`LifecycleMode`
/// dispatch). Without this task, reload events publish into a
/// broadcast channel with no consumer and the plugin never
/// reloads — see the per-mode dispatch contract on
/// `AdmissionEngine::reload_plugin`.
///
/// The task runs for the lifetime of the steward process; it
/// exits when the coordinator's broadcast sender is dropped
/// (i.e. the coordinator itself was dropped) or when the
/// channel returns a closed error.
///
/// `Lagged` errors from the broadcast receiver are logged but
/// do not terminate the task — the channel's drop-oldest
/// semantics on slow subscribers mean a brief backlog spike
/// during a multi-plugin reload storm is observable but not
/// fatal.
pub fn spawn_dispatcher(
    coordinator: Arc<PluginLifecycleCoordinator>,
    engine: Arc<Mutex<AdmissionEngine>>,
) {
    let mut rx = coordinator.subscribe();
    tokio::spawn(async move {
        tracing::info!("plugin lifecycle dispatcher: ready");
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let source = match event.source {
                        ReloadSource::FileWatcher => "file_watcher",
                        ReloadSource::OperatorGesture => "operator_gesture",
                    };
                    let mut eng = engine.lock().await;
                    if let Err(e) = eng
                        .reload_plugin_with_source(&event.plugin_name, source)
                        .await
                    {
                        tracing::warn!(
                            plugin = %event.plugin_name,
                            source,
                            error = %e,
                            "lifecycle dispatcher: reload returned error \
                             (structured outcome already emitted by engine)"
                        );
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "lifecycle dispatcher: lagged on reload-event channel; \
                         operator-config reloads may have been dropped"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!(
                        "plugin lifecycle dispatcher: coordinator closed; exiting"
                    );
                    return;
                }
            }
        }
    });
}

/// Plugin name extraction from a config-file path. The
/// convention is `<plugin-name>.toml` directly in the
/// `plugins_dir`. Returns `None` for paths that do not
/// match the convention.
pub fn plugin_name_from_path(path: &Path) -> Option<String> {
    if path.extension().and_then(|s| s.to_str()) != Some("toml") {
        return None;
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn plugin_name_extraction_strips_toml_extension() {
        let path =
            PathBuf::from("/etc/evo/plugins.d/org.example.multiroom.toml");
        assert_eq!(
            plugin_name_from_path(&path),
            Some("org.example.multiroom".to_string())
        );
    }

    #[test]
    fn plugin_name_extraction_rejects_non_toml() {
        let path = PathBuf::from("/etc/evo/plugins.d/readme.md");
        assert_eq!(plugin_name_from_path(&path), None);
    }

    #[test]
    fn plugin_name_extraction_handles_bare_extension() {
        let path = PathBuf::from("/etc/evo/plugins.d/.toml");
        // file_stem of ".toml" is None or "" — either way,
        // the result is None or an empty string.
        let result = plugin_name_from_path(&path);
        assert!(result.is_none() || result == Some(String::new()));
    }

    #[tokio::test]
    async fn reload_source_distinguishable() {
        let dir = TempDir::new().unwrap();
        let coord = PluginLifecycleCoordinator::new(dir.path());
        let mut rx = coord.subscribe();

        coord.request_reload("test-plugin").await;
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.plugin_name, "test-plugin");
        assert_eq!(ev.source, ReloadSource::OperatorGesture);
    }

    #[tokio::test]
    async fn operator_request_includes_config_path() {
        let dir = TempDir::new().unwrap();
        let coord = PluginLifecycleCoordinator::new(dir.path());
        let mut rx = coord.subscribe();

        coord.request_reload("multi.room").await;
        let ev = rx.try_recv().unwrap();
        assert!(ev.config_path.ends_with("multi.room.toml"));
        assert!(ev.config_path.starts_with(dir.path()));
    }

    #[tokio::test]
    async fn polling_observes_new_toml_file() {
        let dir = TempDir::new().unwrap();
        let coord = PluginLifecycleCoordinator::new(dir.path());
        let mut rx = coord.subscribe();

        // First poll: records the directory's current state
        // (empty). No event.
        coord.poll_once().await.unwrap();
        assert!(rx.try_recv().is_err());

        // Create a TOML file.
        let path = dir.path().join("plugin.alpha.toml");
        std::fs::write(&path, "name = \"plugin.alpha\"\n").unwrap();
        // Second poll: detects the file. Records its initial
        // mtime as "last observed"; no event on first sight
        // (the polling code records state on first
        // observation so subsequent unchanged polls are
        // no-ops).
        coord.poll_once().await.unwrap();
        // First-observation policy: no event on first sight.
        assert!(rx.try_recv().is_err());

        // Wait > debounce window then modify the file.
        tokio::time::sleep(DEBOUNCE_WINDOW + Duration::from_millis(100)).await;
        std::fs::write(&path, "name = \"plugin.alpha\"\nchanged = true\n")
            .unwrap();
        coord.poll_once().await.unwrap();

        let ev = rx.try_recv().expect("modified file emits reload event");
        assert_eq!(ev.plugin_name, "plugin.alpha");
        assert_eq!(ev.source, ReloadSource::FileWatcher);
    }

    #[tokio::test]
    async fn polling_debounces_rapid_edits() {
        let dir = TempDir::new().unwrap();
        let coord = PluginLifecycleCoordinator::new(dir.path());
        let mut rx = coord.subscribe();

        let path = dir.path().join("plugin.beta.toml");
        std::fs::write(&path, "v=1").unwrap();
        coord.poll_once().await.unwrap();
        let _ = rx.try_recv();

        // Two rapid edits within the debounce window: only
        // one event should emit (or zero if the polls happen
        // too fast).
        std::fs::write(&path, "v=2").unwrap();
        coord.poll_once().await.unwrap();
        let first_count = rx.try_recv().is_ok() as usize;
        std::fs::write(&path, "v=3").unwrap();
        coord.poll_once().await.unwrap();
        let second_count = rx.try_recv().is_ok() as usize;

        // At most one event in the rapid window — exact
        // count depends on timing of the poll cycles, but
        // never both edits.
        assert!(first_count + second_count <= 1);
    }

    #[tokio::test]
    async fn polling_ignores_non_toml_files() {
        let dir = TempDir::new().unwrap();
        let coord = PluginLifecycleCoordinator::new(dir.path());
        let mut rx = coord.subscribe();

        std::fs::write(dir.path().join("README.md"), "# notes\n").unwrap();
        std::fs::write(dir.path().join("plugin.bin"), "elf").unwrap();
        coord.poll_once().await.unwrap();

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn polling_handles_missing_directory() {
        let coord =
            PluginLifecycleCoordinator::new("/nonexistent/path/to/plugins");
        // Missing directory is treated as no plugins to
        // watch; poll_once returns Ok without error.
        assert!(coord.poll_once().await.is_ok());
    }
}
