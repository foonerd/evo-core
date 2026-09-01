// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Listener configuration for the HTTPS runtime mount.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Where the HTTPS listener should bind.
#[derive(Debug, Clone)]
pub enum ListenAddr {
    /// IPv4 / IPv6 socket address.
    Socket(SocketAddr),
}

impl ListenAddr {
    /// Build a listener address from any [`std::net::ToSocketAddrs`]
    /// input, taking the first resolution.
    pub fn socket(addr: SocketAddr) -> Self {
        Self::Socket(addr)
    }
}

/// Full configuration the listener consumes at startup.
///
/// The bundle is held in memory; the optional cert-file path
/// enables the hot-reload watcher. If `cert_path` and
/// `key_path` are set, the watcher re-reads both on any
/// change event and swaps the active `rustls::ServerConfig`.
#[derive(Debug, Clone)]
pub struct HttpsListenerConfig {
    /// Where to bind.
    pub listen: ListenAddr,

    /// Cert file path for hot-reload. When set, the listener
    /// watches this file and rebuilds the server config on
    /// change.
    pub cert_path: Option<PathBuf>,

    /// Key file path for hot-reload. Same watcher as
    /// `cert_path`; both files must be present for reload to
    /// succeed.
    pub key_path: Option<PathBuf>,

    /// The URL prefix every wire op binds under. Defaults to
    /// `/api/v1` so paths compose into
    /// `/api/v1/<wire_op_id>`.
    pub api_prefix: String,
}

impl HttpsListenerConfig {
    /// Build with a bind address and the conventional
    /// `/api/v1` prefix; no hot-reload watcher.
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen: ListenAddr::Socket(listen),
            cert_path: None,
            key_path: None,
            api_prefix: "/api/v1".to_string(),
        }
    }

    /// Builder: attach cert + key paths so the listener
    /// hot-reloads on file change.
    pub fn with_hot_reload(
        mut self,
        cert_path: PathBuf,
        key_path: PathBuf,
    ) -> Self {
        self.cert_path = Some(cert_path);
        self.key_path = Some(key_path);
        self
    }

    /// Builder: override the URL prefix.
    pub fn with_api_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.api_prefix = prefix.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn config_defaults_carry_api_v1_prefix() {
        let addr =
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
        let cfg = HttpsListenerConfig::new(addr);
        assert_eq!(cfg.api_prefix, "/api/v1");
        assert!(cfg.cert_path.is_none());
        assert!(cfg.key_path.is_none());
    }

    #[test]
    fn config_builder_attaches_hot_reload_paths() {
        let addr =
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
        let cfg = HttpsListenerConfig::new(addr)
            .with_hot_reload("/etc/cert.pem".into(), "/etc/key.pem".into());
        assert_eq!(cfg.cert_path.as_deref(), Some("/etc/cert.pem".as_ref()));
        assert_eq!(cfg.key_path.as_deref(), Some("/etc/key.pem".as_ref()));
    }

    #[test]
    fn config_builder_overrides_api_prefix() {
        let addr =
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
        let cfg =
            HttpsListenerConfig::new(addr).with_api_prefix("/internal/v2");
        assert_eq!(cfg.api_prefix, "/internal/v2");
    }
}
