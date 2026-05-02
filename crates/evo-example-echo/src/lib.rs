//! # evo-example-echo
//!
//! Example singleton respondent plugin for evo. Echoes input payloads back
//! as output. Used as a fixture by the steward's end-to-end tests and as a
//! reference implementation of the `Plugin` + `Respondent` traits.
//!
//! This plugin is trivially simple on purpose. It demonstrates:
//!
//! - Implementing `Plugin` (describe, load, unload, health_check).
//! - Implementing `Respondent` (handle_request).
//! - Shipping a `manifest.toml` alongside the code.
//! - Emitting structured tracing events per the evo logging contract.
//!
//! The manifest is embedded at compile time and is available via
//! [`manifest`] so the steward can admit this plugin without reading from
//! disk.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;

/// The echo plugin's embedded manifest, as a static string.
///
/// Available so callers can validate the manifest at test time or admit
/// the plugin without disk I/O.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Parse the embedded manifest into a [`Manifest`] struct.
///
/// Panics if the embedded manifest fails to parse. Such a failure is a
/// build-time bug, not a runtime condition, so panicking is acceptable.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("evo-example-echo's embedded manifest must parse")
}

/// The echo plugin.
///
/// A minimal singleton respondent that echoes any `"echo"` request payload
/// back as its response.
#[derive(Debug, Default)]
pub struct EchoPlugin {
    loaded: bool,
    echo_count: u64,
}

impl EchoPlugin {
    /// Construct a new echo plugin instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of echo requests handled since `load` (or since construction
    /// if never loaded). Useful for tests.
    pub fn echo_count(&self) -> u64 {
        self.echo_count
    }
}

impl Plugin for EchoPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: "org.evo.example.echo".to_string(),
                    version: semver::Version::new(0, 1, 1),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec!["echo".to_string()],
                    accepts_custody: false,
                    flags: Default::default(),
                },
                build_info: BuildInfo {
                    plugin_build: env!("CARGO_PKG_VERSION").to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        _ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::info!(plugin = "org.evo.example.echo", "plugin load");
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            tracing::info!(
                plugin = "org.evo.example.echo",
                echoes = self.echo_count,
                "plugin unload"
            );
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("echo plugin not loaded")
            }
        }
    }
}

impl Respondent for EchoPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "echo plugin not loaded".to_string(),
                ));
            }
            if req.request_type != "echo" {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {}",
                    req.request_type
                )));
            }
            self.echo_count += 1;
            tracing::debug!(
                plugin = "org.evo.example.echo",
                cid = req.correlation_id,
                bytes = req.payload.len(),
                "echo"
            );
            Ok(Response::for_request(req, req.payload.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, "org.evo.example.echo");
        assert_eq!(m.plugin.contract, 1);
    }

    #[tokio::test]
    async fn describe_returns_expected_identity() {
        let p = EchoPlugin::new();
        let d = p.describe().await;
        assert_eq!(d.identity.name, "org.evo.example.echo");
        assert_eq!(d.identity.contract, 1);
        assert_eq!(d.runtime_capabilities.request_types, vec!["echo"]);
    }

    #[tokio::test]
    async fn health_is_unhealthy_before_load() {
        let p = EchoPlugin::new();
        let r = p.health_check().await;
        assert!(matches!(
            r.status,
            evo_plugin_sdk::contract::HealthStatus::Unhealthy
        ));
    }

    #[tokio::test]
    async fn handle_request_rejects_before_load() {
        let mut p = EchoPlugin::new();
        let req = Request {
            request_type: "echo".into(),
            payload: b"hi".to_vec(),
            correlation_id: 1,
            deadline: None,
        };
        let e = p.handle_request(&req).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn handle_request_rejects_unknown_type() {
        let mut p = EchoPlugin::new();
        p.loaded = true;
        let req = Request {
            request_type: "shout".into(),
            payload: b"hi".to_vec(),
            correlation_id: 1,
            deadline: None,
        };
        let e = p.handle_request(&req).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn handle_request_echoes_payload() {
        let mut p = EchoPlugin::new();
        p.loaded = true;
        let req = Request {
            request_type: "echo".into(),
            payload: b"hello, evo".to_vec(),
            correlation_id: 42,
            deadline: None,
        };
        let resp = p.handle_request(&req).await.unwrap();
        assert_eq!(resp.payload, b"hello, evo");
        assert_eq!(resp.correlation_id, 42);
        assert_eq!(p.echo_count(), 1);
    }
}
