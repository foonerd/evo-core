//! Sticky endpoint cache.
//!
//! Persists the last-known-good audio-plane endpoint per
//! chain-admitted peer across reboot. Reconnect / dial /
//! probe paths read the cache to target a cached endpoint
//! without depending on mDNS-SD freshness — the library
//! dedupes resolved events on identical-data responses, so
//! the cache provides the authoritative per-peer endpoint
//! lookup independent of the multicast event stream.
//!
//! Updates land on every verified heartbeat receipt (via the
//! [`spawn_heartbeat_autorecorder`] subscriber task) and may
//! land on every successful audio-plane Hello (a future
//! integration point in the audio-plane handshake). The
//! cache is the single substrate-side source of truth for
//! per-peer canonical endpoints.
//!
//! Address-change behaviour: when a heartbeat arrives with
//! an endpoint different from the cached one, the cache
//! records the new endpoint and emits an `EndpointCacheChange`
//! event on its broadcast channel. Operator UI subscribers
//! surface the change; the framework default is auto-accept,
//! valid on a LAN-scope operator-controlled segment.
//! Hostile-network distributions
//! that need explicit endpoint confirmation use the change
//! event to gate the update behind an operator gesture.
//!
//! Resource posture:
//! - Per-peer state: bounded by trust ledger size (~10 LAN
//!   domains).
//! - In-memory cache backed by SQLite write-through; reads
//!   served from cache after first load.
//! - Subscriber broadcast channel capacity 64 (drop-oldest
//!   on slow subscriber per the substrate contract).
//! - SQLite write per address change (rare); SQLite write
//!   per first-observation (~once per peer per boot).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, Mutex, Notify};

use crate::heartbeat::HeartbeatRuntime;
use crate::persistence::{PersistedPeerEndpoint, PersistenceStore};

/// Subscriber broadcast channel capacity. Drop-oldest on slow
/// subscriber per the substrate contract.
const SUBSCRIBER_CAPACITY: usize = 64;

/// Event emitted when the cached endpoint for a peer is set
/// or changed. First-observation produces an event with
/// `previous_endpoint: None`; subsequent changes carry the
/// prior value so operator UI can surface the diff.
#[derive(Debug, Clone)]
pub struct EndpointCacheChange {
    /// Device id of the peer whose endpoint was cached.
    pub device_id: String,
    /// Previously-cached endpoint, or `None` on first
    /// observation.
    pub previous_endpoint: Option<String>,
    /// Newly-cached endpoint.
    pub new_endpoint: String,
    /// Wall-clock time of the change, ms since UNIX epoch.
    pub at_ms: u64,
}

/// Cache errors.
#[derive(Debug, thiserror::Error)]
pub enum EndpointCacheError {
    /// Persistence-layer failure during cache load or write.
    #[error("endpoint cache persistence: {0}")]
    Persistence(#[from] crate::persistence::PersistenceError),
}

/// Sticky endpoint cache runtime.
pub struct EndpointCache {
    persistence: Arc<dyn PersistenceStore>,
    /// In-memory mirror of the persisted cache. Loaded at
    /// construction time; kept consistent with SQLite via
    /// write-through `record()` and `forget()`.
    state: Mutex<HashMap<String, PersistedPeerEndpoint>>,
    change_tx: broadcast::Sender<EndpointCacheChange>,
}

impl EndpointCache {
    /// Construct + rehydrate the cache from persistence.
    /// Returns the cache fully ready for read / write traffic.
    pub async fn load(
        persistence: Arc<dyn PersistenceStore>,
    ) -> Result<Arc<Self>, EndpointCacheError> {
        let rows = persistence.list_peer_endpoints().await?;
        let mut state = HashMap::with_capacity(rows.len());
        for row in rows {
            state.insert(row.device_id.clone(), row);
        }
        let (change_tx, _) = broadcast::channel(SUBSCRIBER_CAPACITY);
        Ok(Arc::new(Self {
            persistence,
            state: Mutex::new(state),
            change_tx,
        }))
    }

    /// Subscribe to address-change events.
    pub fn subscribe(&self) -> broadcast::Receiver<EndpointCacheChange> {
        self.change_tx.subscribe()
    }

    /// Read the cached endpoint for a peer. Returns `None`
    /// when no endpoint has been observed.
    pub async fn get(&self, device_id: &str) -> Option<String> {
        let state = self.state.lock().await;
        state.get(device_id).map(|p| p.last_known_endpoint.clone())
    }

    /// Snapshot every cached endpoint as raw persistence
    /// records. Caller may sort or filter at the substrate-
    /// consumer's discretion.
    pub async fn list(&self) -> Vec<PersistedPeerEndpoint> {
        let state = self.state.lock().await;
        let mut rows: Vec<PersistedPeerEndpoint> =
            state.values().cloned().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_observed_at_ms));
        rows
    }

    /// Record an observed endpoint for a peer. Write-through
    /// to persistence; emits `EndpointCacheChange` on first
    /// observation OR when the endpoint differs from the
    /// cached value. Same-endpoint updates only bump the
    /// `last_observed_at_ms` in cache (no event emitted) and
    /// are NOT written to persistence — the substrate stays
    /// sparse on the disk side (one write per real change,
    /// not one per heartbeat).
    pub async fn record(
        &self,
        device_id: &str,
        endpoint: &str,
    ) -> Result<(), EndpointCacheError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut state = self.state.lock().await;
        let previous_endpoint =
            state.get(device_id).map(|p| p.last_known_endpoint.clone());
        let changed = previous_endpoint.as_deref() != Some(endpoint);
        let record = PersistedPeerEndpoint {
            device_id: device_id.to_string(),
            last_known_endpoint: endpoint.to_string(),
            last_observed_at_ms: now_ms,
        };
        state.insert(device_id.to_string(), record.clone());
        drop(state);

        if changed {
            // Write-through to persistence and emit the
            // change event. Same-endpoint updates skip both
            // — cache stays in-memory only and event channel
            // stays sparse.
            self.persistence.put_peer_endpoint(record).await?;
            let _ = self.change_tx.send(EndpointCacheChange {
                device_id: device_id.to_string(),
                previous_endpoint,
                new_endpoint: endpoint.to_string(),
                at_ms: now_ms,
            });
        }
        Ok(())
    }
}

/// Spawn a task that subscribes to the heartbeat substrate
/// and auto-records every observed peer endpoint into the
/// cache. The task runs until `shutdown` notifies. First-
/// observation and address changes write through to
/// persistence + emit on the change channel; same-endpoint
/// heartbeats are silently coalesced.
///
/// Returns the spawned `JoinHandle` so the caller can compose
/// it into the framework's task supervision tree.
pub fn spawn_heartbeat_autorecorder(
    cache: Arc<EndpointCache>,
    heartbeat: Arc<HeartbeatRuntime>,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = heartbeat.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::debug!(
                        "endpoint cache autorecorder: shutdown received"
                    );
                    return;
                }
                event = rx.recv() => {
                    match event {
                        Ok(ev) => {
                            // First endpoint in the list is
                            // the peer's preferred. Record
                            // just that one; the cache holds
                            // one canonical endpoint per
                            // peer.
                            if let Some(ep) = ev.endpoints.first() {
                                if let Err(e) = cache.record(
                                    &ev.device_id, ep,
                                ).await {
                                    tracing::debug!(
                                        error = %e,
                                        device_id = %ev.device_id,
                                        "endpoint cache autorecorder: record failed"
                                    );
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(
                            n,
                        )) => {
                            tracing::debug!(
                                lagged = n,
                                "endpoint cache autorecorder: subscriber lagged; recovered"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// First-boot polite-probe / dial helper: resolve a peer's
/// last-known endpoint with a bounded wait. If the cache has
/// an entry, return immediately. If empty, wait up to
/// `discovery_timeout` for the autorecorder to populate from
/// an arriving heartbeat. Returns `None` if no heartbeat
/// arrives within the timeout.
///
/// Used by source-host dial paths that need an endpoint before
/// audio fan-out begins — first-boot bootstrap awaits the
/// heartbeat-broadcasted endpoint rather than dialing a stale
/// cached value.
pub async fn wait_for_endpoint(
    cache: Arc<EndpointCache>,
    device_id: &str,
    discovery_timeout: Duration,
) -> Option<String> {
    if let Some(ep) = cache.get(device_id).await {
        return Some(ep);
    }
    let deadline = tokio::time::Instant::now() + discovery_timeout;
    let mut rx = cache.subscribe();
    loop {
        let remaining =
            deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(change)) => {
                if change.device_id == device_id {
                    return Some(change.new_endpoint);
                }
            }
            Ok(Err(_)) | Err(_) => return cache.get(device_id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    #[tokio::test]
    async fn first_observation_writes_through_and_emits() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let cache = EndpointCache::load(Arc::clone(&store)).await.unwrap();
        let mut rx = cache.subscribe();
        cache.record("device-a", "10.0.0.1:7331").await.unwrap();

        // Cache holds it.
        assert_eq!(
            cache.get("device-a").await.as_deref(),
            Some("10.0.0.1:7331")
        );

        // Persistence holds it (write-through).
        let row = store.get_peer_endpoint("device-a").await.unwrap();
        assert_eq!(
            row.as_ref().map(|r| r.last_known_endpoint.as_str()),
            Some("10.0.0.1:7331")
        );

        // Event was emitted with previous_endpoint = None.
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.device_id, "device-a");
        assert_eq!(ev.previous_endpoint, None);
        assert_eq!(ev.new_endpoint, "10.0.0.1:7331");
    }

    #[tokio::test]
    async fn same_endpoint_no_event_no_write() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let cache = EndpointCache::load(Arc::clone(&store)).await.unwrap();

        // First observation: event + write.
        cache.record("device-a", "10.0.0.1:7331").await.unwrap();
        let mut rx = cache.subscribe();
        // Subscribe AFTER first record; channel is now empty.

        // Same endpoint: no event, no second write.
        cache.record("device-a", "10.0.0.1:7331").await.unwrap();
        assert!(rx.try_recv().is_err());
        // Persistence still has the original single row.
        let rows = store.list_peer_endpoints().await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn changed_endpoint_emits_with_previous() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let cache = EndpointCache::load(Arc::clone(&store)).await.unwrap();
        cache.record("device-a", "10.0.0.1:7331").await.unwrap();
        let mut rx = cache.subscribe();

        cache.record("device-a", "10.0.0.2:7331").await.unwrap();

        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.previous_endpoint.as_deref(), Some("10.0.0.1:7331"));
        assert_eq!(ev.new_endpoint, "10.0.0.2:7331");
    }

    #[tokio::test]
    async fn load_rehydrates_from_persistence() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        store
            .put_peer_endpoint(PersistedPeerEndpoint {
                device_id: "device-z".into(),
                last_known_endpoint: "192.0.2.42:7331".into(),
                last_observed_at_ms: 1_700_000_000_000,
            })
            .await
            .unwrap();

        let cache = EndpointCache::load(Arc::clone(&store)).await.unwrap();
        assert_eq!(
            cache.get("device-z").await.as_deref(),
            Some("192.0.2.42:7331")
        );
    }

    #[tokio::test]
    async fn list_returns_most_recent_first() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let cache = EndpointCache::load(Arc::clone(&store)).await.unwrap();

        cache.record("device-a", "10.0.0.1:7331").await.unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        cache.record("device-b", "10.0.0.2:7331").await.unwrap();

        let rows = cache.list().await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].device_id, "device-b");
        assert_eq!(rows[1].device_id, "device-a");
    }

    #[tokio::test]
    async fn wait_for_endpoint_returns_cached_immediately() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let cache = EndpointCache::load(Arc::clone(&store)).await.unwrap();
        cache.record("device-a", "10.0.0.1:7331").await.unwrap();

        let start = tokio::time::Instant::now();
        let ep = wait_for_endpoint(
            Arc::clone(&cache),
            "device-a",
            Duration::from_secs(5),
        )
        .await;
        let elapsed = start.elapsed();
        assert_eq!(ep.as_deref(), Some("10.0.0.1:7331"));
        assert!(elapsed < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn wait_for_endpoint_times_out_when_no_observation() {
        let store: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let cache = EndpointCache::load(Arc::clone(&store)).await.unwrap();

        let ep = wait_for_endpoint(
            Arc::clone(&cache),
            "device-unknown",
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(ep, None);
    }
}
