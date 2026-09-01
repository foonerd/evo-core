// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Shared-clock substrate for multi-room groups.
//!
//! Millisecond-class time alignment between a group's
//! source-host and its receivers — the foundation
//! synchronous playback rests on. Within each group, the
//! elected source-host is the time authority; every receiver
//! tracks an offset against it. The framework owns the typed
//! shape, the per-group state machine, and the operator
//! visibility surface. The actual sync protocol — how
//! receivers measure round-trip time and offset against the
//! source-host — rides the network audio plane (a later
//! sub-primitive); the runtime here exposes
//! [`ClockSyncRuntime::record_sync_sample`] as the write
//! seam that protocol implementation will populate.
//!
//! ## State per group
//!
//! For each multi-room group the local node observes:
//!
//! - When the local node is itself the elected source-host:
//!   [`SyncQuality::Locked`], offset zero, uncertainty zero.
//!   The local clock is the reference; nothing to align.
//! - When the elected source-host is a remote peer and no
//!   sync sample has landed yet: [`SyncQuality::Warming`].
//!   Receivers must not begin synchronous playback until at
//!   least one sample has been measured.
//! - When the elected source-host is a remote peer and a
//!   fresh sample is on file: [`SyncQuality::Locked`] with
//!   the most recent offset + uncertainty.
//! - When the most recent sample for a remote source-host
//!   ages past the configured staleness window:
//!   [`SyncQuality::Stale`]. The offset is still surfaced
//!   for diagnostic visibility but receivers should treat
//!   playback alignment as unreliable.
//! - When no source-host is elected for the group (no live
//!   member): [`SyncQuality::Unknown`].
//!
//! ## State is not persisted
//!
//! Clock state is inherently transient — the local monotonic
//! clock resets on every restart, and any persisted offset
//! is meaningless against a fresh measurement. The runtime
//! is in-memory only; on restart, every group's clock state
//! is rebuilt from the election runtime's
//! [`Happening::SourceHostElected`] notifications.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use evo_primitives::DeviceId;

use crate::groups::GroupStore;
use crate::happenings::{Happening, HappeningBus};

/// Quality classification of one group's clock-sync state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncQuality {
    /// No source-host is elected for the group (no live
    /// member). No sync target.
    Unknown,
    /// A remote peer is the elected source-host, but no
    /// sync sample has been measured yet. Receivers must
    /// not begin synchronous playback until at least one
    /// sample lands.
    Warming,
    /// Either the local node is the source-host (offset
    /// zero, uncertainty zero, by definition), or a remote
    /// source-host has a fresh sync sample within the
    /// runtime's staleness window.
    Locked,
    /// The elected source-host is a remote peer whose most
    /// recent sync sample has aged past the staleness
    /// window. The offset is still surfaced for diagnostic
    /// visibility; receivers should treat alignment as
    /// unreliable until a fresh sample lands.
    Stale,
}

/// Clock-sync state for one multi-room group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockSync {
    /// Canonical group id this state applies to.
    pub group_id: String,
    /// Group's display name (mirrors `GroupRenamed` updates).
    pub display_name: String,
    /// Elected source-host device id. `None` when no
    /// source-host is currently elected.
    pub source_host_device_id: Option<String>,
    /// `true` when the local node is itself the source-host
    /// for this group. When `true`, `offset_ms` and
    /// `uncertainty_ms` are zero by definition.
    pub is_local_host: bool,
    /// Signed millisecond offset between the source-host's
    /// clock and the local clock (positive means the source-
    /// host is ahead). Zero when the local node is the
    /// source-host. Zero when no sample has been measured
    /// yet (in which case `quality` is `Warming`).
    pub offset_ms: i64,
    /// Measurement uncertainty in milliseconds (typically
    /// half the round-trip time of the most recent sample).
    /// Zero when the local node is the source-host. Zero
    /// when no sample has been measured.
    pub uncertainty_ms: u32,
    /// Wall-clock millisecond timestamp the most recent
    /// sample was recorded. Zero when no sample has landed.
    pub last_sync_at_ms: u64,
    /// Quality classification — see [`SyncQuality`].
    pub quality: SyncQuality,
}

/// Configuration for the [`ClockSyncRuntime`].
#[derive(Debug, Clone)]
pub struct ClockSyncConfig {
    /// Cadence of the periodic stale-detector tick. Default
    /// 5 seconds.
    pub eval_interval: Duration,
    /// Time window after which a remote source-host's most
    /// recent sample expires from `Locked` to `Stale`.
    /// Default 30 seconds.
    pub staleness_window: Duration,
}

impl Default for ClockSyncConfig {
    fn default() -> Self {
        Self {
            eval_interval: Duration::from_secs(5),
            staleness_window: Duration::from_secs(30),
        }
    }
}

/// In-memory shared-clock runtime. Owns the per-group state,
/// the happenings subscriber that follows election
/// transitions, and the periodic stale-detector tick.
pub struct ClockSyncRuntime {
    happenings: Arc<HappeningBus>,
    group_store: Arc<GroupStore>,
    local_device_id: DeviceId,
    config: ClockSyncConfig,
    inner: AsyncMutex<ClockSyncInner>,
}

impl std::fmt::Debug for ClockSyncRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClockSyncRuntime")
            .field("local_device_id", &self.local_device_id)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct ClockSyncInner {
    eval_task: Option<JoinHandle<()>>,
    subscriber_task: Option<JoinHandle<()>>,
    /// Per-group clock-sync state, keyed on group_id.
    states: HashMap<String, ClockSync>,
}

impl ClockSyncRuntime {
    /// Construct a runtime. Sync tracking does not begin
    /// until [`Self::start`] is called.
    pub fn new(
        happenings: Arc<HappeningBus>,
        group_store: Arc<GroupStore>,
        local_device_id: DeviceId,
        config: ClockSyncConfig,
    ) -> Self {
        Self {
            happenings,
            group_store,
            local_device_id,
            config,
            inner: AsyncMutex::new(ClockSyncInner::default()),
        }
    }

    /// Start the runtime: spawn the happenings subscriber
    /// (which seeds initial state from `SourceHostElected`
    /// notifications and follows transitions) and the
    /// periodic stale-detector tick.
    pub async fn start(self: &Arc<Self>) {
        let mut g = self.inner.lock().await;
        if g.eval_task.is_some() {
            return;
        }

        let runtime = Arc::clone(self);
        let mut rx = runtime.happenings.subscribe();
        let subscriber_task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(h) => runtime.observe_happening(h).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(
                        _,
                    )) => {
                        // Subscriber fell behind; recover by
                        // reseeding from current group + election
                        // state via group_store.
                        runtime.reseed().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return;
                    }
                }
            }
        });

        let runtime = Arc::clone(self);
        let interval = runtime.config.eval_interval;
        let staleness = runtime.config.staleness_window;
        let eval_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                runtime.evaluate_staleness(now_ms(), staleness).await;
            }
        });

        g.eval_task = Some(eval_task);
        g.subscriber_task = Some(subscriber_task);
    }

    /// Shut the runtime down. Idempotent.
    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        if let Some(t) = g.eval_task.take() {
            t.abort();
        }
        if let Some(t) = g.subscriber_task.take() {
            t.abort();
        }
    }

    /// Read the clock-sync state for one group. Returns
    /// `None` when the group has no recorded state (typically
    /// because no source-host election has fired for it).
    pub async fn get(&self, group_id: &str) -> Option<ClockSync> {
        let g = self.inner.lock().await;
        g.states.get(group_id).cloned()
    }

    /// List every recorded clock-sync state ordered by
    /// group id.
    pub async fn list(&self) -> Vec<ClockSync> {
        let g = self.inner.lock().await;
        let mut rows: Vec<ClockSync> = g.states.values().cloned().collect();
        rows.sort_by(|a, b| a.group_id.cmp(&b.group_id));
        rows
    }

    /// Record a sync sample measured against a remote source-
    /// host. Updates the per-group state and emits a
    /// [`Happening::ClockSyncChanged`] when the quality or
    /// offset materially changes. The sub-primitive that
    /// implements the network sync protocol (a later
    /// component) calls this; the runtime exposes it as the
    /// public seam protocol implementations write through.
    ///
    /// `at_ms` is the wall-clock timestamp the sample was
    /// taken. Samples whose `source_host_device_id` does not
    /// match the currently-elected source-host are dropped
    /// (a stale measurement after a failover).
    pub async fn record_sync_sample(
        &self,
        group_id: &str,
        source_host_device_id: &str,
        offset_ms: i64,
        uncertainty_ms: u32,
        at_ms: u64,
    ) {
        let happening = {
            let mut g = self.inner.lock().await;
            let Some(state) = g.states.get_mut(group_id) else {
                return;
            };
            if state.is_local_host {
                return;
            }
            if state
                .source_host_device_id
                .as_deref()
                .is_none_or(|cur| cur != source_host_device_id)
            {
                return;
            }
            let prior_quality = state.quality;
            let prior_offset = state.offset_ms;
            state.offset_ms = offset_ms;
            state.uncertainty_ms = uncertainty_ms;
            state.last_sync_at_ms = at_ms;
            state.quality = SyncQuality::Locked;
            if prior_quality == SyncQuality::Locked && prior_offset == offset_ms
            {
                None
            } else {
                Some(Happening::ClockSyncChanged {
                    group_id: state.group_id.clone(),
                    display_name: state.display_name.clone(),
                    source_host_device_id: state.source_host_device_id.clone(),
                    is_local_host: false,
                    offset_ms,
                    uncertainty_ms,
                    quality: serialise_quality(state.quality),
                    at: std::time::SystemTime::now(),
                })
            }
        };
        if let Some(h) = happening {
            self.emit(h).await;
        }
    }

    /// Apply a `SourceHostElected` transition. Public so
    /// integration callers (and tests) can drive the
    /// runtime without going through the bus.
    pub async fn apply_source_host_election(
        &self,
        group_id: &str,
        display_name: &str,
        source_host_device_id: Option<&str>,
    ) {
        let happening = {
            let mut g = self.inner.lock().await;
            let is_local = matches!(
                source_host_device_id,
                Some(id) if id == self.local_device_id.0
            );
            let new_quality = match source_host_device_id {
                None => SyncQuality::Unknown,
                Some(_) if is_local => SyncQuality::Locked,
                Some(_) => SyncQuality::Warming,
            };
            let prior_quality = g.states.get(group_id).map(|s| s.quality);
            let entry =
                g.states.entry(group_id.to_string()).or_insert_with(|| {
                    ClockSync {
                        group_id: group_id.to_string(),
                        display_name: display_name.to_string(),
                        source_host_device_id: None,
                        is_local_host: false,
                        offset_ms: 0,
                        uncertainty_ms: 0,
                        last_sync_at_ms: 0,
                        quality: SyncQuality::Unknown,
                    }
                });
            entry.display_name = display_name.to_string();
            entry.source_host_device_id =
                source_host_device_id.map(str::to_string);
            entry.is_local_host = is_local;
            entry.offset_ms = 0;
            entry.uncertainty_ms = 0;
            entry.last_sync_at_ms = if is_local { now_ms() } else { 0 };
            entry.quality = new_quality;
            if prior_quality == Some(new_quality) {
                None
            } else {
                Some(Happening::ClockSyncChanged {
                    group_id: entry.group_id.clone(),
                    display_name: entry.display_name.clone(),
                    source_host_device_id: entry.source_host_device_id.clone(),
                    is_local_host: is_local,
                    offset_ms: entry.offset_ms,
                    uncertainty_ms: entry.uncertainty_ms,
                    quality: serialise_quality(entry.quality),
                    at: std::time::SystemTime::now(),
                })
            }
        };
        if let Some(h) = happening {
            self.emit(h).await;
        }
    }

    /// Drop the per-group state when the group itself is
    /// deleted. Public so integration callers (and tests) can
    /// drive the runtime without going through the bus.
    pub async fn forget_group(&self, group_id: &str) {
        let mut g = self.inner.lock().await;
        g.states.remove(group_id);
    }

    /// Walk every recorded state and flip remote-host
    /// `Locked` entries to `Stale` when their
    /// `last_sync_at_ms` ages past the staleness window.
    /// Idempotent. Public for testing without waiting for
    /// the periodic tick.
    pub async fn evaluate_staleness(&self, now: u64, staleness: Duration) {
        let cutoff = now.saturating_sub(staleness.as_millis() as u64);
        let mut transitions: Vec<Happening> = Vec::new();
        {
            let mut g = self.inner.lock().await;
            for state in g.states.values_mut() {
                if state.is_local_host || state.quality != SyncQuality::Locked {
                    continue;
                }
                if state.last_sync_at_ms == 0 {
                    continue;
                }
                if state.last_sync_at_ms < cutoff {
                    state.quality = SyncQuality::Stale;
                    transitions.push(Happening::ClockSyncChanged {
                        group_id: state.group_id.clone(),
                        display_name: state.display_name.clone(),
                        source_host_device_id: state
                            .source_host_device_id
                            .clone(),
                        is_local_host: false,
                        offset_ms: state.offset_ms,
                        uncertainty_ms: state.uncertainty_ms,
                        quality: serialise_quality(state.quality),
                        at: std::time::SystemTime::now(),
                    });
                }
            }
        }
        for h in transitions {
            self.emit(h).await;
        }
    }

    async fn observe_happening(&self, h: Happening) {
        match h {
            Happening::SourceHostElected {
                group_id,
                display_name,
                source_host_device_id,
                ..
            } => {
                self.apply_source_host_election(
                    &group_id,
                    &display_name,
                    source_host_device_id.as_deref(),
                )
                .await;
            }
            Happening::GroupDeleted { group_id, .. } => {
                self.forget_group(&group_id).await;
            }
            Happening::GroupRenamed {
                group_id,
                display_name,
                ..
            } => {
                let mut g = self.inner.lock().await;
                if let Some(state) = g.states.get_mut(&group_id) {
                    state.display_name = display_name;
                }
            }
            _ => {}
        }
    }

    /// Reseed the in-memory state from `group_store`. Used
    /// when the happenings subscriber falls behind and
    /// recovers — the snapshot is rebuilt against current
    /// truth. Source-host state is left at its prior value
    /// per group; subsequent `SourceHostElected`
    /// notifications correct it.
    async fn reseed(&self) {
        let groups = match self.group_store.list().await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "clock sync: reseed failed (group store list)"
                );
                return;
            }
        };
        let live_ids: std::collections::HashSet<String> =
            groups.iter().map(|g| g.group_id.0.clone()).collect();
        let mut g = self.inner.lock().await;
        g.states.retain(|k, _| live_ids.contains(k));
        for group in groups {
            g.states.entry(group.group_id.0.clone()).or_insert_with(|| {
                ClockSync {
                    group_id: group.group_id.0.clone(),
                    display_name: group.display_name.clone(),
                    source_host_device_id: None,
                    is_local_host: false,
                    offset_ms: 0,
                    uncertainty_ms: 0,
                    last_sync_at_ms: 0,
                    quality: SyncQuality::Unknown,
                }
            });
        }
    }

    async fn emit(&self, h: Happening) {
        if let Err(e) = self.happenings.emit_durable(h).await {
            tracing::warn!(
                error = %e,
                "clock sync: emit happening failed"
            );
        }
    }
}

fn serialise_quality(q: SyncQuality) -> String {
    match q {
        SyncQuality::Unknown => "unknown".to_string(),
        SyncQuality::Warming => "warming".to_string(),
        SyncQuality::Locked => "locked".to_string(),
        SyncQuality::Stale => "stale".to_string(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{MemoryPersistenceStore, PersistenceStore};

    fn build_runtime(local_id: &str) -> Arc<ClockSyncRuntime> {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let bus = Arc::new(HappeningBus::with_capacity(64));
        let groups = Arc::new(GroupStore::new(
            Arc::clone(&persistence),
            Arc::clone(&bus),
        ));
        Arc::new(ClockSyncRuntime::new(
            bus,
            groups,
            DeviceId(local_id.to_string()),
            ClockSyncConfig::default(),
        ))
    }

    #[tokio::test]
    async fn election_for_local_host_sets_locked_with_zero_offset() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("local-id"))
            .await;
        let state = runtime.get("g1").await.unwrap();
        assert_eq!(state.quality, SyncQuality::Locked);
        assert!(state.is_local_host);
        assert_eq!(state.offset_ms, 0);
        assert_eq!(state.uncertainty_ms, 0);
    }

    #[tokio::test]
    async fn election_for_remote_host_sets_warming() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("remote-id"))
            .await;
        let state = runtime.get("g1").await.unwrap();
        assert_eq!(state.quality, SyncQuality::Warming);
        assert!(!state.is_local_host);
        assert_eq!(state.source_host_device_id.as_deref(), Some("remote-id"));
    }

    #[tokio::test]
    async fn election_with_no_host_sets_unknown() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", None)
            .await;
        let state = runtime.get("g1").await.unwrap();
        assert_eq!(state.quality, SyncQuality::Unknown);
        assert!(!state.is_local_host);
        assert!(state.source_host_device_id.is_none());
    }

    #[tokio::test]
    async fn record_sync_sample_locks_warming_remote() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("remote-id"))
            .await;
        runtime
            .record_sync_sample("g1", "remote-id", 12, 3, 1_000_000)
            .await;
        let state = runtime.get("g1").await.unwrap();
        assert_eq!(state.quality, SyncQuality::Locked);
        assert_eq!(state.offset_ms, 12);
        assert_eq!(state.uncertainty_ms, 3);
        assert_eq!(state.last_sync_at_ms, 1_000_000);
    }

    #[tokio::test]
    async fn record_sync_sample_drops_when_source_host_mismatches() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("remote-a"))
            .await;
        runtime
            .record_sync_sample("g1", "remote-b", 99, 1, 1_000_000)
            .await;
        let state = runtime.get("g1").await.unwrap();
        // Sample dropped — quality stays warming.
        assert_eq!(state.quality, SyncQuality::Warming);
    }

    #[tokio::test]
    async fn record_sync_sample_is_no_op_when_local_is_host() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("local-id"))
            .await;
        // Even if a stray sample comes in claiming the local
        // id, ignore it — there is nothing to synchronise.
        runtime
            .record_sync_sample("g1", "local-id", 999, 9, 1_000_000)
            .await;
        let state = runtime.get("g1").await.unwrap();
        assert_eq!(state.offset_ms, 0);
        assert_eq!(state.uncertainty_ms, 0);
    }

    #[tokio::test]
    async fn evaluate_staleness_flips_locked_to_stale() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("remote-id"))
            .await;
        runtime
            .record_sync_sample("g1", "remote-id", 5, 2, 1_000)
            .await;
        // Now is far past the sample's last_sync_at_ms +
        // staleness window.
        runtime
            .evaluate_staleness(1_000_000, Duration::from_secs(30))
            .await;
        let state = runtime.get("g1").await.unwrap();
        assert_eq!(state.quality, SyncQuality::Stale);
        // Offset still surfaced for diagnostic visibility.
        assert_eq!(state.offset_ms, 5);
    }

    #[tokio::test]
    async fn evaluate_staleness_skips_local_host() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("local-id"))
            .await;
        runtime
            .evaluate_staleness(1_000_000_000, Duration::from_secs(30))
            .await;
        let state = runtime.get("g1").await.unwrap();
        // Local host stays Locked irrespective of clock age.
        assert_eq!(state.quality, SyncQuality::Locked);
    }

    #[tokio::test]
    async fn forget_group_drops_state() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("local-id"))
            .await;
        assert!(runtime.get("g1").await.is_some());
        runtime.forget_group("g1").await;
        assert!(runtime.get("g1").await.is_none());
    }

    #[tokio::test]
    async fn election_to_remote_resets_offset_to_warming() {
        let runtime = build_runtime("local-id");
        runtime
            .apply_source_host_election("g1", "Lounge", Some("remote-a"))
            .await;
        runtime
            .record_sync_sample("g1", "remote-a", 7, 1, 1_000)
            .await;
        // Now election flips to a different remote.
        runtime
            .apply_source_host_election("g1", "Lounge", Some("remote-b"))
            .await;
        let state = runtime.get("g1").await.unwrap();
        // New host: state resets to Warming with zero offset.
        assert_eq!(state.quality, SyncQuality::Warming);
        assert_eq!(state.offset_ms, 0);
        assert_eq!(state.uncertainty_ms, 0);
        assert_eq!(state.last_sync_at_ms, 0);
        assert_eq!(state.source_host_device_id.as_deref(), Some("remote-b"));
    }
}
