// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Gateway plugin registry.
//!
//! A *gateway* plugin bridges between an external ecosystem
//! (AirPlay 2, Cast, Spotify Connect, Roon Ready, MusicCast,
//! HEOS, Sonos, etc.) and the multi-room native protocol.
//! The framework recognises gateway plugins through their
//! manifest declaration
//! ([`evo_plugin_sdk::manifest::GatewayCapabilities`]) at
//! admission time and tracks them in the in-memory
//! [`GatewayRegistry`] for operator visibility.
//!
//! Gateway implementations are vendor-distribution scope:
//! the framework's reference generic-device build ships
//! only legally-clean plugins, while vendor distributions
//! that hold the upstream ecosystem's certification carry
//! the gateway plugins for their target ecosystems. The
//! framework substrate is the contract + registry; the
//! plugins are the consumers.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use evo_plugin_sdk::manifest::GatewayDirection;

/// Operator-visible record for one admitted gateway plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayInfo {
    /// Canonical plugin name (reverse-DNS).
    pub plugin_name: String,
    /// Vendor-defined protocol identifier (e.g.
    /// `airplay2`, `chromecast`, `roon-ready`,
    /// `spotify-connect`).
    pub protocol: String,
    /// Bridge direction.
    pub direction: GatewayDirection,
    /// Whether the protocol is licensed / certified by the
    /// upstream ecosystem owner.
    pub licensed: bool,
    /// Wall-clock millisecond timestamp the plugin was
    /// registered (typically at admission).
    pub registered_at_ms: u64,
}

/// In-memory registry of admitted gateway plugins.
///
/// Populated at plugin admission time; cleared on plugin
/// removal. Cheap to share via `Arc`. Operator surfaces
/// query `list()` to render the current registry.
#[derive(Debug, Default)]
pub struct GatewayRegistry {
    inner: AsyncMutex<HashMap<String, GatewayInfo>>,
}

impl GatewayRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a shared registry handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Register a gateway plugin. Idempotent on the plugin
    /// name — a re-register replaces the prior entry (e.g.
    /// after a manifest reload).
    pub async fn register(&self, info: GatewayInfo) {
        let mut g = self.inner.lock().await;
        g.insert(info.plugin_name.clone(), info);
    }

    /// Remove a gateway plugin by canonical name. Idempotent
    /// on absent names.
    pub async fn unregister(&self, plugin_name: &str) {
        let mut g = self.inner.lock().await;
        g.remove(plugin_name);
    }

    /// List every registered gateway plugin, ordered by
    /// canonical plugin name.
    pub async fn list(&self) -> Vec<GatewayInfo> {
        let g = self.inner.lock().await;
        let mut rows: Vec<GatewayInfo> = g.values().cloned().collect();
        rows.sort_by(|a, b| a.plugin_name.cmp(&b.plugin_name));
        rows
    }

    /// Look up one gateway plugin by canonical name.
    pub async fn get(&self, plugin_name: &str) -> Option<GatewayInfo> {
        let g = self.inner.lock().await;
        g.get(plugin_name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(
        name: &str,
        protocol: &str,
        direction: GatewayDirection,
        licensed: bool,
    ) -> GatewayInfo {
        GatewayInfo {
            plugin_name: name.to_string(),
            protocol: protocol.to_string(),
            direction,
            licensed,
            registered_at_ms: 1_000,
        }
    }

    #[tokio::test]
    async fn register_and_list() {
        let r = GatewayRegistry::new();
        r.register(info(
            "com.example.airplay",
            "airplay2",
            GatewayDirection::Bidirectional,
            true,
        ))
        .await;
        let rows = r.list().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].plugin_name, "com.example.airplay");
        assert_eq!(rows[0].protocol, "airplay2");
    }

    #[tokio::test]
    async fn register_is_idempotent_on_plugin_name() {
        let r = GatewayRegistry::new();
        r.register(info(
            "com.example.cast",
            "chromecast",
            GatewayDirection::OutboundSink,
            false,
        ))
        .await;
        r.register(info(
            "com.example.cast",
            "chromecast",
            GatewayDirection::OutboundSink,
            true,
        ))
        .await;
        let rows = r.list().await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].licensed);
    }

    #[tokio::test]
    async fn list_orders_by_plugin_name() {
        let r = GatewayRegistry::new();
        r.register(info(
            "com.zzzz.late",
            "p1",
            GatewayDirection::InboundSource,
            false,
        ))
        .await;
        r.register(info(
            "com.aaaa.early",
            "p2",
            GatewayDirection::InboundSource,
            false,
        ))
        .await;
        let rows = r.list().await;
        assert_eq!(rows[0].plugin_name, "com.aaaa.early");
        assert_eq!(rows[1].plugin_name, "com.zzzz.late");
    }

    #[tokio::test]
    async fn unregister_removes() {
        let r = GatewayRegistry::new();
        r.register(info(
            "com.example.roon",
            "roon-ready",
            GatewayDirection::OutboundSink,
            true,
        ))
        .await;
        assert_eq!(r.list().await.len(), 1);
        r.unregister("com.example.roon").await;
        assert!(r.list().await.is_empty());
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown() {
        let r = GatewayRegistry::new();
        assert!(r.get("nope").await.is_none());
    }
}
