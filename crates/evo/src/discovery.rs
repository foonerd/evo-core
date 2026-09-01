// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Multi-room peer discovery via mDNS Service Discovery.
//!
//! Each evo device advertises itself on the local broadcast
//! domain under the service type
//! [`EVO_SERVICE_TYPE`] (`_evo._tcp.local.`) carrying a TXT
//! record with the canonical [`evo_primitives::DeviceId`],
//! the operator-editable display name, the optional
//! vendor-distribution identifier, the framework version, the
//! advertised capability flags, and an optional truncated
//! SHA-256 fingerprint of the vendor-supplied public key (when
//! the vendor's cryptographic-services hook populated one).
//!
//! Browsing nodes catalogue every other evo device they
//! observe in the [`PersistedDiscoveredPeer`] substrate. The
//! catalogue survives a steward restart so a brief network
//! blip after boot does not erase the operator's view of the
//! peer set; the runtime's TTL prune loop garbage-collects
//! rows whose `last_seen_ms` ages past the configured peer-
//! TTL window so the substrate does not grow without bound
//! across long operating life.
//!
//! State transitions emit [`crate::happenings::Happening`]
//! variants `PeerDiscovered`, `PeerUpdated`, and `PeerLost`
//! (label-coalesced on the canonical id), so subscribers
//! observe a deduplicated stream when peers re-announce
//! repeatedly.
//!
//! Subsequent multi-room sub-primitives (group entity,
//! source-host election, network audio plane, verb targeting)
//! consume this peer set as their candidate pool.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use evo_primitives::DeviceId;

use crate::happenings::HappeningBus;
use crate::persistence::{
    PersistedDiscoveredPeer, PersistenceError, PersistenceStore,
};
use evo_primitives::DeviceIdentity;

/// Canonical service type the framework advertises and
/// browses on. Conforms to RFC 6763 §4.1: type `evo`,
/// transport `tcp`, domain `local.` Trailing dot is required
/// by the mdns-sd library.
pub const EVO_SERVICE_TYPE: &str = "_evo._tcp.local.";

/// TXT-record key for the canonical device id (UUIDv4 token
/// form).
pub const TXT_KEY_DEVICE_ID: &str = "id";
/// TXT-record key for the operator-editable display name.
pub const TXT_KEY_DISPLAY_NAME: &str = "name";
/// TXT-record key for the vendor-distribution identifier.
pub const TXT_KEY_VENDOR: &str = "vendor";
/// TXT-record key for the framework version string.
pub const TXT_KEY_VERSION: &str = "version";
/// TXT-record key for the comma-separated capability flag
/// list.
pub const TXT_KEY_CAPABILITIES: &str = "caps";
/// TXT-record key for the URL-safe base64-encoded truncated
/// SHA-256 fingerprint of the public key. Omitted when the
/// vendor distribution does not populate the key slot.
pub const TXT_KEY_PUBLIC_KEY_FINGERPRINT: &str = "pkfp";

// Note: the peer's full 32-byte Ed25519 verifying key does
// NOT ride the mDNS-SD TXT record. Public-key delivery is
// the canonical responsibility of the UDP/5354 chain-
// announce envelope (broadcast carrier, signed payload).
// `discovered_peers.public_key_b64` is populated from the
// announce-discovery bridge alone, keeping one source of
// truth for the key per Team Pin's one-canonical-path-per-
// capability rule.

/// The framework version string baked into every
/// advertisement TXT record. Matches the crate version so a
/// peer advertising `version=X.Y.Z` is running release X.Y.Z
/// of the evo framework.
pub const FRAMEWORK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Length of the truncated SHA-256 public-key fingerprint
/// (16 bytes — sufficient to disambiguate rare key
/// collisions while keeping the TXT record under the 255-
/// byte per-record cap).
pub const PUBLIC_KEY_FINGERPRINT_LEN: usize = 16;

/// Errors raised by [`DiscoveryRuntime`].
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// Underlying persistence error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// Underlying mDNS-SD library error.
    #[error("mDNS daemon error: {0}")]
    Mdns(String),
    /// Invariant the discovery runtime relies on was violated
    /// by upstream input — typically a malformed TXT record
    /// from a non-evo responder happening to share the
    /// service type.
    #[error("discovery invariant violated: {0}")]
    Invariant(String),
}

impl From<mdns_sd::Error> for DiscoveryError {
    fn from(e: mdns_sd::Error) -> Self {
        DiscoveryError::Mdns(e.to_string())
    }
}

/// One entry in the baseline snapshot
/// [`DiscoveryRuntime::snapshot_baseline`] returns. Carries
/// the substrate-row fields the roster-snap orchestrator
/// consults on departed-peer rows: the most recent
/// last-seen timestamp, observed socket-addr strings, and
/// the operator-editable display name. The display name
/// covers the case where a peer's substrate row is pruned
/// between baseline capture and snap composition — the
/// baseline is the only place the human-readable name
/// survives in that window, and the snap response would
/// otherwise have to fall back to the canonical id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEntry {
    /// Substrate-recorded `last_seen_ms` for the peer.
    pub last_seen_ms: u64,
    /// Socket-addr strings (`host:port`) the substrate row
    /// carried at baseline-capture time. Marauder queries
    /// use these as the TCP-connect target set.
    pub addresses: Vec<String>,
    /// Operator-editable display name as the peer most
    /// recently advertised. Falls back to the canonical id
    /// only if the baseline row itself was somehow
    /// constructed without a name (no observed instance in
    /// production).
    pub display_name: String,
}

/// Configuration for the [`DiscoveryRuntime`].
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Whether discovery is enabled. When `false`,
    /// [`DiscoveryRuntime::start`] returns a no-op handle and
    /// the daemon is never constructed.
    pub enabled: bool,
    /// Reserved control-plane TCP port advertised in the
    /// `SRV` record. Subsequent multi-room sub-primitives bind
    /// the actual TCP listener here; the discovery
    /// advertisement refers ahead to that port so peer nodes
    /// can pre-populate their connection target.
    pub control_port: u16,
    /// Capability flag strings to advertise in the `caps=`
    /// TXT field. Built at boot from observed plugin shelves
    /// and runtime state — the framework declares
    /// `multi-room` unconditionally; vendors may extend.
    pub capability_flags: Vec<String>,
    /// Time window after which a peer that has not re-
    /// announced is considered lost. Default 300 seconds (5
    /// minutes) — long enough to ride out a network blip,
    /// short enough that a removed device disappears from
    /// the operator surface within a coffee break.
    pub peer_ttl: Duration,
    /// Cadence of the TTL prune loop. Default 30 seconds.
    pub prune_interval: Duration,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            control_port: 7331,
            capability_flags: vec!["multi-room".to_string()],
            peer_ttl: Duration::from_secs(300),
            prune_interval: Duration::from_secs(30),
        }
    }
}

/// In-memory snapshot of a discovered peer. Mirrors
/// [`PersistedDiscoveredPeer`] one-to-one; the in-memory
/// shape is what the substrate persists.
pub type DiscoveredPeer = PersistedDiscoveredPeer;

/// Compute the truncated SHA-256 fingerprint used in the
/// TXT record. Returns the first
/// [`PUBLIC_KEY_FINGERPRINT_LEN`] bytes of the SHA-256
/// digest of the input.
pub fn fingerprint_public_key(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest[..PUBLIC_KEY_FINGERPRINT_LEN].to_vec()
}

/// Encode the truncated fingerprint as URL-safe base64
/// without padding. The encoding is used for the TXT-record
/// `pkfp=` value so the TXT record stays printable ASCII.
pub fn encode_fingerprint_b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a base64-encoded fingerprint from the TXT record.
fn decode_fingerprint_b64(s: &str) -> Result<Vec<u8>, DiscoveryError> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| DiscoveryError::Invariant(format!("invalid pkfp: {e}")))
}

/// Persistence-backed discovery runtime. Owns the
/// [`mdns_sd::ServiceDaemon`], the bridge task that drains
/// the daemon's [`ServiceEvent`] receiver, the TTL prune
/// loop, and the peer substrate handle.
pub struct DiscoveryRuntime {
    persistence: Arc<dyn PersistenceStore>,
    happenings: Arc<HappeningBus>,
    config: DiscoveryConfig,
    inner: AsyncMutex<DiscoveryInner>,
}

impl std::fmt::Debug for DiscoveryRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryRuntime")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct DiscoveryInner {
    daemon: Option<ServiceDaemon>,
    advertised_fullname: Option<String>,
    bridge_task: Option<JoinHandle<()>>,
    prune_task: Option<JoinHandle<()>>,
    /// In-memory mirror of the peer set, keyed on canonical
    /// id. The substrate is the source of truth; this is a
    /// fast read path for `list_peers()` and the set the
    /// bridge mutates atomically alongside the substrate.
    peers: HashMap<String, DiscoveredPeer>,
    /// Maps the mDNS-SD fullname (`<instance>.<service>`) to
    /// the resolved canonical device id. Populated on
    /// `ServiceResolved` and consulted on `ServiceRemoved`
    /// — the removal event carries only the fullname, so
    /// the canonical id (and the in-memory peer row keyed
    /// on it) cannot be recovered without this index.
    fullname_to_id: HashMap<String, String>,
}

impl DiscoveryRuntime {
    /// Construct an idle runtime. Discovery does not start
    /// until [`Self::start`] is called.
    pub fn new(
        persistence: Arc<dyn PersistenceStore>,
        happenings: Arc<HappeningBus>,
        config: DiscoveryConfig,
    ) -> Self {
        Self {
            persistence,
            happenings,
            config,
            inner: AsyncMutex::new(DiscoveryInner::default()),
        }
    }

    /// Rehydrate the in-memory peer set from the substrate.
    /// Called once at boot before `start()` so the TTL prune
    /// loop has a populated view to work over.
    pub async fn rehydrate(&self) -> Result<(), DiscoveryError> {
        let rows = self.persistence.list_discovered_peers().await?;
        let mut g = self.inner.lock().await;
        g.peers = rows.into_iter().map(|p| (p.device_id.clone(), p)).collect();
        Ok(())
    }

    /// Start the daemon, register the local advertisement,
    /// begin browsing for peers, and spawn the TTL prune
    /// loop. No-op when [`DiscoveryConfig::enabled`] is
    /// `false`.
    pub async fn start(
        self: &Arc<Self>,
        identity: &DeviceIdentity,
    ) -> Result<(), DiscoveryError> {
        if !self.config.enabled {
            return Ok(());
        }
        let mut g = self.inner.lock().await;
        if g.daemon.is_some() {
            return Ok(());
        }

        let daemon = ServiceDaemon::new()?;

        let txt = build_local_txt(identity, &self.config.capability_flags);
        let host_name = format!("{}.local.", instance_label_for(identity));
        let info = ServiceInfo::new(
            EVO_SERVICE_TYPE,
            &instance_label_for(identity),
            &host_name,
            "",
            self.config.control_port,
            txt,
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;

        let receiver = daemon.browse(EVO_SERVICE_TYPE)?;

        let runtime = Arc::clone(self);
        let local_id = identity.device_id.clone();
        let bridge = tokio::task::spawn(async move {
            run_bridge(runtime, receiver, local_id).await;
        });

        let runtime = Arc::clone(self);
        let prune = tokio::task::spawn(async move {
            run_prune(runtime).await;
        });

        g.daemon = Some(daemon);
        g.advertised_fullname = Some(fullname);
        g.bridge_task = Some(bridge);
        g.prune_task = Some(prune);
        Ok(())
    }

    /// Shut the daemon down. Idempotent.
    pub async fn shutdown(&self) {
        let (daemon, fullname, bridge, prune) = {
            let mut g = self.inner.lock().await;
            (
                g.daemon.take(),
                g.advertised_fullname.take(),
                g.bridge_task.take(),
                g.prune_task.take(),
            )
        };
        if let (Some(daemon), Some(fullname)) = (daemon.as_ref(), fullname) {
            let _ = daemon.unregister(&fullname);
        }
        if let Some(daemon) = daemon {
            let _ = daemon.shutdown();
        }
        if let Some(b) = bridge {
            b.abort();
        }
        if let Some(p) = prune {
            p.abort();
        }
    }

    /// List every peer currently observed. Order is
    /// last-seen descending — most-recently-observed first.
    pub async fn list_peers(&self) -> Vec<DiscoveredPeer> {
        let g = self.inner.lock().await;
        let mut out: Vec<DiscoveredPeer> = g.peers.values().cloned().collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.last_seen_ms));
        out
    }

    /// Re-register the local mDNS-SD advertisement against the
    /// freshly-persisted [`DeviceIdentity`].
    ///
    /// Unregisters the prior advert (by recorded fullname) and
    /// registers a new [`ServiceInfo`] with a TXT record rebuilt
    /// from [`build_local_txt`]. Updates the recorded fullname
    /// under the runtime's lock so subsequent `shutdown` /
    /// `re_advertise` calls target the new advert.
    ///
    /// No-op when [`DiscoveryConfig::enabled`] is `false` or the
    /// daemon has not started yet — both reflect a runtime
    /// where the advert never went on the wire to begin with.
    /// Returns `Ok(())` in both no-op cases so callers can
    /// invoke this unconditionally from rename / collision
    /// paths without precondition checks.
    ///
    /// The unregister + register sequence is not atomic at the
    /// wire level; observers may briefly see the device
    /// disappear before the new advert lands. In practice the
    /// peer-prune TTL is sized comfortably above this window.
    pub async fn re_advertise(
        &self,
        identity: &DeviceIdentity,
    ) -> Result<(), DiscoveryError> {
        if !self.config.enabled {
            return Ok(());
        }
        let mut g = self.inner.lock().await;
        if g.daemon.is_none() {
            return Ok(());
        }

        let prior_fullname = g.advertised_fullname.take();
        let txt = build_local_txt(identity, &self.config.capability_flags);
        let host_name = format!("{}.local.", instance_label_for(identity));
        let info = ServiceInfo::new(
            EVO_SERVICE_TYPE,
            &instance_label_for(identity),
            &host_name,
            "",
            self.config.control_port,
            txt,
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        let daemon = g.daemon.as_ref().expect("daemon checked above");
        if let Some(prior) = prior_fullname {
            let _ = daemon.unregister(&prior);
        }
        daemon.register(info)?;
        g.advertised_fullname = Some(fullname);
        Ok(())
    }

    /// Capture a baseline snapshot of every currently-known
    /// peer indexed by canonical id. Each [`BaselineEntry`]
    /// carries the peer's last-seen timestamp, observed
    /// addresses, and the operator-editable display name the
    /// peer most recently advertised. The roster-snap
    /// orchestrator uses the baseline as the before-image
    /// against which post-snap substrate state is compared,
    /// uses the recorded addresses as the marauder-query
    /// input set when the substrate drops a previously-known
    /// peer between baseline and snap composition, and uses
    /// the display name to render departed-peer rows in the
    /// snap response without falling back to the canonical
    /// id (the operator-visible row would otherwise lose the
    /// human-readable name on a peer whose substrate row was
    /// pruned mid-snap).
    ///
    /// Reading is O(n) under a brief read lock; the lock is
    /// released before the snap orchestrator waits any
    /// further timing window.
    pub async fn snapshot_baseline(&self) -> HashMap<String, BaselineEntry> {
        let g = self.inner.lock().await;
        g.peers
            .iter()
            .map(|(id, p)| {
                (
                    id.clone(),
                    BaselineEntry {
                        last_seen_ms: p.last_seen_ms,
                        addresses: p.addresses.clone(),
                        display_name: p.display_name.clone(),
                    },
                )
            })
            .collect()
    }

    /// Marauder query: TCP-connect-probe a set of candidate
    /// peers against their recorded addresses. Returns the
    /// subset of `device_id`s whose `(host:port)` socket
    /// accepted a connection within the supplied deadline.
    ///
    /// The roster-snap orchestrator uses this as the
    /// authoritative reachability check: if a previously-
    /// known peer has dropped from the substrate but a TCP
    /// connect to its advertised control port succeeds, the
    /// peer is reclassified as present in the snap response;
    /// if the connect fails (timeout, refused, network
    /// unreachable), the absence is marauder-confirmed.
    ///
    /// Each connect attempt runs with its own bounded
    /// deadline (`per_probe_deadline`). Probes run
    /// concurrently so the marauder window stays bounded by
    /// the slowest single probe (not the sum of all probes).
    /// Network failures are silent (peer simply is not
    /// reported reachable); structured-error propagation up
    /// the snap path is not load-bearing — the absence of
    /// `device_id` in the returned set carries the same
    /// meaning as a probe error.
    pub async fn probe_peers_tcp(
        targets: &[(String, String)],
        per_probe_deadline: Duration,
    ) -> std::collections::HashSet<String> {
        let mut tasks = Vec::with_capacity(targets.len());
        for (device_id, addr) in targets {
            let device_id = device_id.clone();
            let addr = addr.clone();
            tasks.push(tokio::spawn(async move {
                let connect = tokio::net::TcpStream::connect(&addr);
                match tokio::time::timeout(per_probe_deadline, connect).await {
                    Ok(Ok(_stream)) => Some(device_id),
                    Ok(Err(_)) | Err(_) => None,
                }
            }));
        }
        let mut reached = std::collections::HashSet::with_capacity(tasks.len());
        for t in tasks {
            if let Ok(Some(id)) = t.await {
                reached.insert(id);
            }
        }
        reached
    }
}

/// Build the TXT-record property map for the local
/// advertisement.
fn build_local_txt(
    identity: &DeviceIdentity,
    capability_flags: &[String],
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    map.insert(
        TXT_KEY_DEVICE_ID.to_string(),
        identity.device_id.as_str().to_string(),
    );
    map.insert(
        TXT_KEY_DISPLAY_NAME.to_string(),
        identity.display_name.clone(),
    );
    if let Some(v) = identity.vendor_id.as_ref() {
        map.insert(TXT_KEY_VENDOR.to_string(), v.clone());
    }
    map.insert(TXT_KEY_VERSION.to_string(), FRAMEWORK_VERSION.to_string());
    map.insert(TXT_KEY_CAPABILITIES.to_string(), capability_flags.join(","));
    if let Some(pk) = identity.public_key_bytes.as_ref() {
        let fp = fingerprint_public_key(pk);
        map.insert(
            TXT_KEY_PUBLIC_KEY_FINGERPRINT.to_string(),
            encode_fingerprint_b64(&fp),
        );
    }
    map
}

/// Build the mDNS-SD instance label from the device's display
/// name. The mdns-sd library copes with collision suffixes
/// internally; we just hand it the human-readable label.
/// Strip characters that mdns-sd refuses (`,`, `=`, control
/// chars) so a free-form display name like `Listening Room
/// (main)` advertises cleanly.
fn instance_label_for(identity: &DeviceIdentity) -> String {
    sanitise_label(&identity.display_name)
        .unwrap_or_else(|| format!("evo-{}", identity.device_id.short()))
}

fn sanitise_label(s: &str) -> Option<String> {
    let cleaned: String = s
        .chars()
        .filter(|c| {
            !c.is_control()
                && *c != ','
                && *c != '='
                && *c != '\u{0}'
                && *c != '.'
        })
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Drain the mDNS-SD event stream into the substrate +
/// happenings bus. Runs until the channel closes.
async fn run_bridge(
    runtime: Arc<DiscoveryRuntime>,
    receiver: mdns_sd::Receiver<ServiceEvent>,
    local_id: DeviceId,
) {
    loop {
        let event = match tokio::task::block_in_place(|| receiver.recv()) {
            Ok(e) => e,
            Err(_) => return,
        };
        match event {
            ServiceEvent::ServiceResolved(info) => {
                if let Err(e) = runtime.handle_resolved(&info, &local_id).await
                {
                    tracing::warn!(error = %e, "discovery bridge: handle_resolved failed");
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                if let Err(e) = runtime.handle_removed(&fullname).await {
                    tracing::warn!(error = %e, "discovery bridge: handle_removed failed");
                }
            }
            ServiceEvent::SearchStarted(_)
            | ServiceEvent::ServiceFound(_, _)
            | ServiceEvent::SearchStopped(_) => {}
            _ => {}
        }
    }
}

impl DiscoveryRuntime {
    async fn handle_resolved(
        &self,
        info: &ResolvedService,
        local_id: &DeviceId,
    ) -> Result<(), DiscoveryError> {
        let device_id = info
            .get_property_val_str(TXT_KEY_DEVICE_ID)
            .ok_or_else(|| {
                DiscoveryError::Invariant(format!(
                    "advertisement {} missing {} TXT field",
                    info.get_fullname(),
                    TXT_KEY_DEVICE_ID
                ))
            })?
            .to_string();
        if device_id == local_id.as_str() {
            return Ok(());
        }
        let display_name = info
            .get_property_val_str(TXT_KEY_DISPLAY_NAME)
            .unwrap_or("")
            .to_string();
        let vendor_id = info
            .get_property_val_str(TXT_KEY_VENDOR)
            .map(|s| s.to_string());
        let framework_version = info
            .get_property_val_str(TXT_KEY_VERSION)
            .map(|s| s.to_string());
        let capability_flags: Vec<String> = info
            .get_property_val_str(TXT_KEY_CAPABILITIES)
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let public_key_fingerprint = info
            .get_property_val_str(TXT_KEY_PUBLIC_KEY_FINGERPRINT)
            .map(decode_fingerprint_b64)
            .transpose()?;

        let mut addresses: Vec<String> = info
            .get_addresses()
            .iter()
            .map(|s| s.to_ip_addr().to_string())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        addresses.sort();
        let port = info.get_port();
        let addresses: Vec<String> = addresses
            .into_iter()
            .map(|a| format!("{a}:{port}"))
            .collect();

        let now_ms = now_ms();
        let fullname = info.get_fullname().to_string();

        let mut g = self.inner.lock().await;
        let (first_seen_ms, prior) = match g.peers.get(&device_id) {
            Some(p) => (p.first_seen_ms, Some(p.clone())),
            None => (now_ms, None),
        };
        // mDNS-SD does not carry the peer's full public key
        // (that travels via the UDP/5354 chain-announce
        // envelope). Preserve any previously-cached key on
        // upsert: a later mDNS observation must not erase
        // what the announce-discovery bridge wrote.
        let public_key_b64 =
            prior.as_ref().and_then(|p| p.public_key_b64.clone());
        let updated = PersistedDiscoveredPeer {
            device_id: device_id.clone(),
            display_name: display_name.clone(),
            addresses: addresses.clone(),
            vendor_id: vendor_id.clone(),
            public_key_fingerprint,
            capability_flags,
            framework_version,
            first_seen_ms,
            last_seen_ms: now_ms,
            public_key_b64,
            // Discovery-only construction: presence + network
            // are projected at read time by
            // `handle_list_discovered_peers` from the chain-
            // scope correlator + endpoint history. Never
            // persisted on the discovery row.
            presence_state: None,
            last_transition_at_ms: None,
            network: None,
        };
        g.peers.insert(device_id.clone(), updated.clone());
        g.fullname_to_id.insert(fullname, device_id.clone());
        drop(g);

        // Persist debounce: every mDNS-SD ServiceResolved fires
        // a resolve callback, but the only field that advances
        // on a pure keepalive refresh is `last_seen_ms`. Writing
        // to persistence on every refresh produces steady-state
        // write amplification (one row-update per peer per
        // mDNS-SD re-resolve cycle, ~every few seconds). Persist
        // only when the substantive state has actually changed,
        // mirroring the `peer_changed()` predicate the emit side
        // already uses. The in-memory map above always reflects
        // the freshest `last_seen_ms` for the duration the peer
        // is reachable; persistence is the durable record of
        // identity + capabilities, not of liveness clock state.
        let persist_required = match &prior {
            None => true,
            Some(prev) => peer_changed(prev, &updated),
        };
        if persist_required {
            self.persistence
                .put_discovered_peer(updated.clone())
                .await?;
        }

        // Emit the event-channel-driven `PeerAnnounced`
        // signal on every ServiceResolved arrival, regardless
        // of whether the peer was already known. Consumers
        // tracking the always-on event-driven roster
        // subscribe to this; consumers that care only about
        // novel observations subscribe to `PeerDiscovered`
        // instead.
        let first_observation = prior.is_none();
        let announced = crate::happenings::Happening::PeerAnnounced {
            device_id: device_id.clone(),
            display_name: display_name.clone(),
            addresses: addresses.clone(),
            first_observation,
            at: std::time::SystemTime::now(),
        };
        if let Err(e) = self.happenings.emit_durable(announced).await {
            tracing::warn!(
                error = %e,
                device_id = %device_id,
                "discovery: emit peer_announced happening failed"
            );
        }

        let happening = match prior {
            None => crate::happenings::Happening::PeerDiscovered {
                device_id: device_id.clone(),
                display_name: display_name.clone(),
                addresses: addresses.clone(),
                at: std::time::SystemTime::now(),
            },
            Some(prev) if peer_changed(&prev, &updated) => {
                crate::happenings::Happening::PeerUpdated {
                    device_id: device_id.clone(),
                    display_name: display_name.clone(),
                    addresses: addresses.clone(),
                    at: std::time::SystemTime::now(),
                }
            }
            Some(_) => return Ok(()),
        };
        if let Err(e) = self.happenings.emit_durable(happening).await {
            tracing::warn!(
                error = %e,
                device_id = %device_id,
                "discovery: emit happening failed"
            );
        }

        // The trust ledger's display-name field for an
        // admitted peer is now chain-canonical: the chain's
        // `AdmitPeer` carries the operator-confirmed name at
        // admit time, and an operator-issued
        // `RenamePeerDisplayName` chain witness is the only
        // way to change it. Discovery does not bridge mDNS-
        // observed names into the trust ledger any more —
        // peers freely advertise whatever local label they
        // hold via mDNS-SD, and `discovered_peers.display_name`
        // tracks the live broadcast independent of the chain
        // record.
        //
        // Collision resolver. When the just-observed peer's
        // display_name matches our local Auto-sourced name, the
        // operator's domain has two devices presenting the
        // same label. Rewrite the local name to
        // `<current>-<id-short>` and re-advertise so peers
        // disambiguate within a discovery roundtrip. Idempotent
        // (the resolver's `display_name.ends_with("-<id-short>")`
        // guard prevents double-suffixing), and a no-op for
        // operator-sourced names (sticky — the resolver
        // does not rewrite them, so the operator's chosen
        // label survives every subsequent collision and only
        // changes when the operator explicitly renames or
        // resets to auto).
        if !display_name.is_empty() {
            let store = crate::device_identity::DeviceIdentityStore::new(
                Arc::clone(&self.persistence),
            );
            match store.get().await {
                Ok(Some(local))
                    if local.display_name == display_name
                        && local.name_source
                            == evo_primitives::NameSource::Auto =>
                {
                    match store.resolve_collision().await {
                        Ok((resolved, true)) => {
                            // Re-register the mDNS-SD advert
                            // with the suffixed name + fan out
                            // the change so remote UIs catch
                            // up reactively.
                            if let Err(e) = self.re_advertise(&resolved).await {
                                tracing::warn!(
                                    error = %e,
                                    device_id = %resolved.device_id.as_str(),
                                    "discovery: collision-resolver \
                                     re-advertise failed"
                                );
                            }
                            let happening =
                                crate::happenings::Happening::DeviceDisplayNameChanged {
                                    device_id: resolved
                                        .device_id
                                        .as_str()
                                        .to_string(),
                                    display_name: resolved
                                        .display_name
                                        .clone(),
                                    at: std::time::SystemTime::now(),
                                };
                            if let Err(e) =
                                self.happenings.emit_durable(happening).await
                            {
                                tracing::warn!(
                                    error = %e,
                                    device_id = %resolved.device_id.as_str(),
                                    "discovery: collision-resolver \
                                     happening emit failed"
                                );
                            }
                        }
                        Ok((_, false)) => {
                            // Already suffixed (or operator-set
                            // — the second guard above already
                            // filters that case, but the
                            // resolver's idempotency makes the
                            // belt-and-braces check cheap).
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "discovery: collision-resolver \
                                 persistence failed"
                            );
                        }
                    }
                }
                Ok(_) => {
                    // Local is operator-sourced or display_name
                    // does not match: no action.
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "discovery: collision-resolver could not read \
                         local identity"
                    );
                }
            }
        }
        Ok(())
    }

    /// Upsert a peer row from a verified chain-announce
    /// observation. Called by the discovery-freshness pump on
    /// every UDP/5354 envelope that self-attests successfully.
    ///
    /// Carriers are complementary: mDNS-SD `ServiceResolved`
    /// populates the operator-visible fields (display_name,
    /// vendor, version, capability flags) via `handle_resolved`;
    /// chain-announce populates the `public_key_b64` and acts as
    /// the canonical freshness clock (refreshing `last_seen_ms`
    /// every 1 Hz). When chain-announce arrives for a peer
    /// mDNS-SD has not yet observed, this method creates a
    /// row carrying device_id + addresses + public_key_b64,
    /// leaving display_name empty until mDNS resolves; when
    /// it arrives for a peer with an existing mDNS row, the
    /// display_name + capability set are preserved.
    pub async fn refresh_from_announce(
        &self,
        envelope_device_id: &str,
        envelope_public_key_b64: &str,
        envelope_endpoints: &[evo_witness::NetworkEndpoint],
        source_addr: std::net::SocketAddr,
        local_id: &DeviceId,
    ) -> Result<(), DiscoveryError> {
        if envelope_device_id == local_id.as_str() {
            return Ok(());
        }
        let now_ms = now_ms();
        let envelope_port = envelope_endpoints
            .first()
            .map(|e| e.port)
            .unwrap_or(source_addr.port());
        let mut addresses: Vec<String> = envelope_endpoints
            .iter()
            .map(|e| format!("{}:{}", e.address, e.port))
            .collect();
        let from_source = format!("{}:{}", source_addr.ip(), envelope_port);
        if !addresses.contains(&from_source) {
            addresses.push(from_source);
        }
        addresses.sort();
        addresses.dedup();

        let mut g = self.inner.lock().await;
        let (
            first_seen_ms,
            display_name,
            vendor_id,
            capability_flags,
            framework_version,
            public_key_fingerprint,
        ) = match g.peers.get(envelope_device_id) {
            Some(prior) => (
                prior.first_seen_ms,
                prior.display_name.clone(),
                prior.vendor_id.clone(),
                prior.capability_flags.clone(),
                prior.framework_version.clone(),
                prior.public_key_fingerprint.clone(),
            ),
            None => (now_ms, String::new(), None, Vec::new(), None, None),
        };
        let updated = PersistedDiscoveredPeer {
            device_id: envelope_device_id.to_string(),
            display_name,
            addresses,
            vendor_id,
            public_key_fingerprint,
            capability_flags,
            framework_version,
            first_seen_ms,
            last_seen_ms: now_ms,
            public_key_b64: Some(envelope_public_key_b64.to_string()),
            // Discovery-only construction: presence + network
            // are projected at read time by
            // `handle_list_discovered_peers`. See the parallel
            // site in this file for the rationale.
            presence_state: None,
            last_transition_at_ms: None,
            network: None,
        };
        let prior = g
            .peers
            .insert(envelope_device_id.to_string(), updated.clone());
        drop(g);
        // Persist debounce: chain-announce fires 1 Hz per peer
        // and would otherwise write a row-update every second
        // forever. Persistence is the durable record of
        // identity + capabilities + public key — not of the
        // liveness clock. In-memory `last_seen_ms` always
        // reflects the latest observation; on-disk
        // `last_seen_ms` lags but no consumer derives liveness
        // from it after boot (the in-memory map is the read
        // surface). Persist only when a substantive field
        // changed.
        let persist_required = match &prior {
            None => true,
            Some(prev) => peer_changed(prev, &updated),
        };
        if persist_required {
            self.persistence.put_discovered_peer(updated).await?;
        }
        Ok(())
    }

    async fn handle_removed(
        &self,
        fullname: &str,
    ) -> Result<(), DiscoveryError> {
        // mdns-sd does not echo the TXT record on removal —
        // the event carries only the fullname. Resolve the
        // canonical id via the `fullname_to_id` index that
        // `handle_resolved` populates.
        let (device_id, display_name) = {
            let mut g = self.inner.lock().await;
            let Some(device_id) = g.fullname_to_id.remove(fullname) else {
                return Ok(());
            };
            let display_name = g
                .peers
                .remove(&device_id)
                .map(|p| p.display_name)
                .unwrap_or_default();
            (device_id, display_name)
        };
        self.persistence.delete_discovered_peer(&device_id).await?;
        let h = crate::happenings::Happening::PeerLost {
            device_id,
            display_name,
            at: std::time::SystemTime::now(),
        };
        if let Err(e) = self.happenings.emit_durable(h).await {
            tracing::warn!(
                error = %e,
                "discovery: emit PeerLost failed"
            );
        }
        Ok(())
    }
}

fn peer_changed(
    prev: &PersistedDiscoveredPeer,
    next: &PersistedDiscoveredPeer,
) -> bool {
    prev.display_name != next.display_name
        || prev.addresses != next.addresses
        || prev.vendor_id != next.vendor_id
        || prev.public_key_fingerprint != next.public_key_fingerprint
        || prev.capability_flags != next.capability_flags
        || prev.framework_version != next.framework_version
}

/// TTL prune loop: every `prune_interval`, walk the in-
/// memory set and delete peers whose last_seen_ms is older
/// than `peer_ttl` ago.
async fn run_prune(runtime: Arc<DiscoveryRuntime>) {
    let interval = runtime.config.prune_interval;
    let ttl_ms = runtime.config.peer_ttl.as_millis() as u64;
    loop {
        tokio::time::sleep(interval).await;
        let cutoff = now_ms().saturating_sub(ttl_ms);
        let stale: Vec<(String, String)> = {
            let g = runtime.inner.lock().await;
            g.peers
                .values()
                .filter(|p| p.last_seen_ms < cutoff)
                .map(|p| (p.device_id.clone(), p.display_name.clone()))
                .collect()
        };
        if stale.is_empty() {
            continue;
        }
        let mut g = runtime.inner.lock().await;
        for (id, _) in &stale {
            g.peers.remove(id);
            g.fullname_to_id.retain(|_, v| v != id);
        }
        drop(g);
        for (id, display_name) in stale {
            if let Err(e) =
                runtime.persistence.delete_discovered_peer(&id).await
            {
                tracing::warn!(
                    error = %e,
                    device_id = %id,
                    "discovery prune: delete failed"
                );
                continue;
            }
            let h = crate::happenings::Happening::PeerLost {
                device_id: id,
                display_name,
                at: std::time::SystemTime::now(),
            };
            if let Err(e) = runtime.happenings.emit_durable(h).await {
                tracing::warn!(
                    error = %e,
                    "discovery prune: emit PeerLost failed"
                );
            }
        }
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
    use evo_primitives::DeviceIdentity;

    fn identity_with_pk(pk: Option<Vec<u8>>) -> DeviceIdentity {
        DeviceIdentity {
            device_id: DeviceId(
                "550e8400-e29b-41d4-a716-446655440000".to_string(),
            ),
            display_name: "Listening Room".to_string(),
            vendor_id: Some("test-vendor".to_string()),
            public_key_bytes: pk,
            created_at_ms: 1000,
            name_source: evo_primitives::NameSource::Auto,
        }
    }

    #[test]
    fn build_local_txt_carries_required_fields() {
        let id = identity_with_pk(None);
        let txt = build_local_txt(&id, &["multi-room".to_string()]);
        assert_eq!(
            txt.get(TXT_KEY_DEVICE_ID).unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(txt.get(TXT_KEY_DISPLAY_NAME).unwrap(), "Listening Room");
        assert_eq!(txt.get(TXT_KEY_VENDOR).unwrap(), "test-vendor");
        assert_eq!(txt.get(TXT_KEY_VERSION).unwrap(), FRAMEWORK_VERSION);
        assert_eq!(txt.get(TXT_KEY_CAPABILITIES).unwrap(), "multi-room");
        assert!(!txt.contains_key(TXT_KEY_PUBLIC_KEY_FINGERPRINT));
    }

    #[test]
    fn build_local_txt_includes_fingerprint_when_pk_present() {
        let id = identity_with_pk(Some(vec![1u8; 32]));
        let txt = build_local_txt(&id, &["multi-room".to_string()]);
        assert!(txt.contains_key(TXT_KEY_PUBLIC_KEY_FINGERPRINT));
    }

    #[test]
    fn fingerprint_is_truncated_sha256() {
        let fp = fingerprint_public_key(b"hello");
        assert_eq!(fp.len(), PUBLIC_KEY_FINGERPRINT_LEN);
    }

    #[test]
    fn fingerprint_b64_roundtrip() {
        let fp = fingerprint_public_key(b"hello");
        let s = encode_fingerprint_b64(&fp);
        let back = decode_fingerprint_b64(&s).unwrap();
        assert_eq!(back, fp);
    }

    #[test]
    fn sanitise_label_strips_dots_and_equals_and_commas() {
        assert_eq!(
            sanitise_label("Living Room (main), unit=A").as_deref(),
            Some("Living Room (main) unitA")
        );
        assert_eq!(sanitise_label("...").as_deref(), None);
        assert_eq!(sanitise_label("    ").as_deref(), None);
    }

    #[test]
    fn instance_label_falls_back_to_device_short() {
        let id = DeviceIdentity {
            device_id: DeviceId(
                "550e8400-e29b-41d4-a716-446655440000".to_string(),
            ),
            display_name: ".".to_string(),
            vendor_id: None,
            public_key_bytes: None,
            created_at_ms: 0,
            name_source: evo_primitives::NameSource::Auto,
        };
        assert_eq!(instance_label_for(&id), "evo-550e8400");
    }

    #[test]
    fn peer_changed_detects_address_drift() {
        let now = now_ms();
        let base = PersistedDiscoveredPeer {
            device_id: "x".into(),
            display_name: "x".into(),
            addresses: vec!["10.0.0.1:7331".into()],
            vendor_id: None,
            public_key_fingerprint: None,
            capability_flags: vec![],
            framework_version: None,
            first_seen_ms: now,
            last_seen_ms: now,
            public_key_b64: None,
            presence_state: None,
            last_transition_at_ms: None,
            network: None,
        };
        let mut next = base.clone();
        next.addresses.push("10.0.0.2:7331".into());
        assert!(peer_changed(&base, &next));
    }

    #[test]
    fn peer_changed_ignores_timestamp_only_drift() {
        let now = now_ms();
        let base = PersistedDiscoveredPeer {
            device_id: "x".into(),
            display_name: "x".into(),
            addresses: vec!["10.0.0.1:7331".into()],
            vendor_id: None,
            public_key_fingerprint: None,
            capability_flags: vec![],
            framework_version: None,
            first_seen_ms: now,
            last_seen_ms: now,
            public_key_b64: None,
            presence_state: None,
            last_transition_at_ms: None,
            network: None,
        };
        let mut next = base.clone();
        next.last_seen_ms = now + 10_000;
        assert!(!peer_changed(&base, &next));
    }

    #[test]
    fn discovery_config_default_enabled() {
        let c = DiscoveryConfig::default();
        assert!(c.enabled);
        assert_eq!(c.control_port, 7331);
        assert_eq!(c.peer_ttl, Duration::from_secs(300));
        assert!(c.capability_flags.contains(&"multi-room".to_string()));
    }

    #[tokio::test]
    async fn rehydrate_loads_substrate_into_memory() {
        use crate::persistence::MemoryPersistenceStore;
        let persistence = Arc::new(MemoryPersistenceStore::default());
        let now = now_ms();
        persistence
            .put_discovered_peer(PersistedDiscoveredPeer {
                device_id: "peer-a".into(),
                display_name: "Peer A".into(),
                addresses: vec!["10.0.0.1:7331".into()],
                vendor_id: None,
                public_key_fingerprint: None,
                capability_flags: vec!["multi-room".into()],
                framework_version: Some("0.1.13".into()),
                first_seen_ms: now,
                last_seen_ms: now,
                public_key_b64: None,
                presence_state: None,
                last_transition_at_ms: None,
                network: None,
            })
            .await
            .unwrap();
        let runtime = DiscoveryRuntime::new(
            persistence,
            Arc::new(HappeningBus::with_capacity(64)),
            DiscoveryConfig::default(),
        );
        runtime.rehydrate().await.unwrap();
        let peers = runtime.list_peers().await;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].device_id, "peer-a");
    }
}
