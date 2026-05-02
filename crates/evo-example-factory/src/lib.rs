//! # evo-example-factory
//!
//! Example factory respondent plugin for evo. Announces a small set of
//! instances at load time, then echoes per-instance request payloads
//! back as responses. Used as a fixture by the steward's end-to-end
//! tests for the factory admission path and as a reference
//! implementation of the `Plugin + Factory + Respondent` shape.
//!
//! Demonstrates:
//!
//! - Implementing `Plugin` (describe, load, unload, health_check).
//! - Implementing `Factory` (retraction_policy).
//! - Implementing `Respondent` (handle_request).
//! - Calling `LoadContext::instance_announcer.announce(...)` from
//!   inside `load` to register a set of instances with the steward.
//! - Shipping a `manifest.toml` declaring `kind.instance = "factory"`.
//!
//! The plugin is intentionally small. It announces three instances —
//! `instance-a`, `instance-b`, `instance-c` — at load time, with
//! arbitrary capability payloads. The retraction policy is
//! [`RetractionPolicy::StartupOnly`]: instances are announced during
//! load and retracted only when the steward unloads the plugin.
//!
//! The manifest is embedded at compile time and is available via
//! [`manifest`] so the steward can admit this plugin without reading
//! from disk.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use evo_plugin_sdk::contract::factory::{
    Factory, InstanceAnnouncement, RetractionPolicy,
};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use std::future::Future;

/// The factory plugin's embedded manifest, as a static string.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Canonical plugin name. Mirrored from the manifest so the wire bin
/// can construct a `HostConfig` without re-parsing.
pub const PLUGIN_NAME: &str = "org.evo.example.factory";

/// Instance IDs the plugin announces at load time. Stable across
/// restarts; the steward uses them as the external addressing for
/// each instance subject.
pub const INSTANCE_IDS: &[&str] = &["instance-a", "instance-b", "instance-c"];

/// Parse the embedded manifest into a [`Manifest`] struct.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("evo-example-factory's embedded manifest must parse")
}

/// The factory plugin.
///
/// On load, announces every entry in [`INSTANCE_IDS`] through the
/// `LoadContext::instance_announcer`. The retraction policy is
/// [`RetractionPolicy::StartupOnly`]; the steward retracts each
/// instance during its drain pass on plugin unload.
#[derive(Debug, Default)]
pub struct ExampleFactoryPlugin {
    loaded: bool,
    request_count: u64,
}

impl ExampleFactoryPlugin {
    /// Construct a new factory plugin instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of requests handled since `load`.
    pub fn request_count(&self) -> u64 {
        self.request_count
    }
}

impl Plugin for ExampleFactoryPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: semver::Version::new(0, 1, 0),
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
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::info!(plugin = PLUGIN_NAME, "plugin load");
            for id in INSTANCE_IDS {
                let announcement =
                    InstanceAnnouncement::new(*id, capability_payload_for(id));
                ctx.instance_announcer
                    .announce(announcement)
                    .await
                    .map_err(|e| {
                        PluginError::Permanent(format!(
                            "{PLUGIN_NAME}: announce_instance({id}) refused: {e}"
                        ))
                    })?;
            }
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            tracing::info!(
                plugin = PLUGIN_NAME,
                requests = self.request_count,
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
                HealthReport::unhealthy("factory plugin not loaded")
            }
        }
    }
}

impl Factory for ExampleFactoryPlugin {
    fn retraction_policy(&self) -> RetractionPolicy {
        // Instances are announced once at load and live until unload.
        // The steward retracts every instance via its lifecycle-drain
        // pass when the plugin is unloaded.
        RetractionPolicy::StartupOnly
    }
}

impl Respondent for ExampleFactoryPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "factory plugin not loaded".to_string(),
                ));
            }
            if req.request_type != "echo" {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {}",
                    req.request_type
                )));
            }
            self.request_count += 1;
            Ok(Response::for_request(req, req.payload.clone()))
        }
    }
}

/// Synthesise a small capability payload for a given instance id.
/// The format is illustrative — a real factory plugin's payload
/// schema is defined by the target shelf.
fn capability_payload_for(id: &str) -> Vec<u8> {
    let body = format!("{{\"id\":\"{id}\",\"caps\":[\"echo\"]}}");
    body.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.contract, 1);
    }

    #[tokio::test]
    async fn describe_returns_expected_identity() {
        let p = ExampleFactoryPlugin::new();
        let d = p.describe().await;
        assert_eq!(d.identity.name, PLUGIN_NAME);
        assert_eq!(d.identity.contract, 1);
        assert_eq!(d.runtime_capabilities.request_types, vec!["echo"]);
    }

    #[test]
    fn retraction_policy_is_startup_only() {
        let p = ExampleFactoryPlugin::new();
        assert!(matches!(
            p.retraction_policy(),
            RetractionPolicy::StartupOnly
        ));
    }

    #[test]
    fn capability_payload_includes_instance_id() {
        let payload = capability_payload_for("instance-a");
        let s = std::str::from_utf8(&payload).expect("payload is utf-8");
        assert!(s.contains("instance-a"));
        assert!(s.contains("echo"));
    }
}
