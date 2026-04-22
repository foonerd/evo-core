//! Plugin manifest types.
//!
//! The types in this module mirror the manifest schema documented in
//! `docs/engineering/PLUGIN_PACKAGING.md` section 2. Every plugin ships with a
//! `manifest.toml` that deserialises into [`Manifest`]; the steward validates
//! the resulting value before admitting the plugin.
//!
//! ## Validation layers
//!
//! Manifest validation happens in three layers:
//!
//! 1. **TOML parse**: Handled by `toml` + `serde`. Missing required fields and
//!    malformed types are rejected here as [`ManifestError::ParseError`].
//! 2. **Schema validation**: [`Manifest::validate`] checks constraints that
//!    cannot be expressed in the type system alone: reverse-DNS name format,
//!    supported contract version, capability-vs-kind consistency.
//! 3. **Shelf-shape validation**: Performed by the steward, not by this SDK.
//!    The steward loads the published shelf-shape schema for the manifest's
//!    `target.shelf` and validates additional domain-specific constraints
//!    declared in that schema. This SDK does not know about specific shelves.
//!
//! [`Manifest::from_toml`] performs layers 1 and 2 in sequence.

use crate::error::ManifestError;
use once_cell::sync::Lazy;
use regex::Regex;
use semver::Version;
use serde::{Deserialize, Serialize};

/// The plugin contract version this SDK supports.
///
/// The SDK admits manifests declaring `plugin.contract = 1`. Manifests
/// declaring any other value are rejected with
/// [`ManifestError::UnsupportedContractVersion`].
pub const SUPPORTED_CONTRACT_VERSION: u32 = 1;

/// Regex matching a valid plugin canonical name.
///
/// The pattern is taken verbatim from `PLUGIN_PACKAGING.md` section 4:
/// `^[a-z][a-z0-9]*(\.[a-z][a-z0-9-]*)+$`.
static NAME_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-z][a-z0-9]*(\.[a-z][a-z0-9-]*)+$")
        .expect("plugin name regex must compile")
});

/// A complete plugin manifest, modelling every section of the schema in
/// `PLUGIN_PACKAGING.md` section 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Identity and version of the plugin.
    pub plugin: Plugin,
    /// The shelf this plugin targets.
    pub target: Target,
    /// The plugin's instance and interaction shapes.
    pub kind: Kind,
    /// How the steward loads this plugin.
    pub transport: Transport,
    /// Declared trust class.
    pub trust: Trust,
    /// Environmental prerequisites for admission.
    pub prerequisites: Prerequisites,
    /// Declared resource ceilings.
    pub resources: Resources,
    /// Lifecycle policy (hot reload, restart, autostart).
    pub lifecycle: Lifecycle,
    /// Kind-specific capability declarations.
    ///
    /// The sub-tables populated here must be consistent with [`Kind`]: a
    /// warden plugin must populate `warden`, a factory must populate
    /// `factory`, a respondent must populate `respondent`. Consistency is
    /// checked by [`Manifest::validate`].
    #[serde(default)]
    pub capabilities: Capabilities,
}

impl Manifest {
    /// Parse a manifest from a TOML string and fully validate it.
    ///
    /// Performs layers 1 and 2 of the validation cascade documented at module
    /// level: TOML parsing followed by schema validation.
    pub fn from_toml(input: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest = toml::from_str(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Serialise this manifest as a TOML string.
    ///
    /// Does not re-validate. Callers that mutate a manifest in memory and
    /// want to ensure the output is still valid should call [`validate`]
    /// before serialising.
    ///
    /// [`validate`]: Manifest::validate
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Validate this manifest against the schema-level constraints that
    /// cannot be expressed as serde-level types.
    ///
    /// Checks:
    /// - `plugin.name` matches the reverse-DNS regex.
    /// - `plugin.contract` equals [`SUPPORTED_CONTRACT_VERSION`].
    /// - `capabilities` sub-tables are consistent with `kind`.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !NAME_REGEX.is_match(&self.plugin.name) {
            return Err(ManifestError::InvalidName(self.plugin.name.clone()));
        }

        if self.plugin.contract != SUPPORTED_CONTRACT_VERSION {
            return Err(ManifestError::UnsupportedContractVersion(
                self.plugin.contract,
            ));
        }

        self.capabilities.validate(&self.kind)?;

        Ok(())
    }
}

/// The `[plugin]` section: identity and version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plugin {
    /// Canonical reverse-DNS name, e.g. `com.fiio.dacs`.
    pub name: String,
    /// Plugin version. Semver.
    pub version: Version,
    /// Plugin contract version this manifest targets. Currently only `1` is
    /// supported by this SDK.
    pub contract: u32,
}

/// The `[target]` section: which shelf this plugin stocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Target {
    /// Fully qualified shelf name, e.g. `metadata.providers`.
    pub shelf: String,
    /// Shelf shape version this plugin satisfies.
    pub shape: u32,
}

/// The `[kind]` section: the plugin's instance and interaction shapes.
///
/// These two axes are orthogonal per `CONCEPT.md` section 5 and
/// `PLUGIN_CONTRACT.md` section 1. Their combination determines which
/// capability sub-tables must be present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Kind {
    /// How many instances of this plugin can exist over time.
    pub instance: InstanceShape,
    /// How the plugin interacts with the steward.
    pub interaction: InteractionShape,
}

/// Instance shape: whether the plugin provides one contribution or many over
/// time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum InstanceShape {
    /// One contribution for the life of the plugin.
    Singleton,
    /// Variable contributions over time, driven by world events.
    Factory,
}

/// Interaction shape: whether the plugin handles discrete requests or takes
/// sustained custody of work.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum InteractionShape {
    /// Handles discrete request-response exchanges.
    Respondent,
    /// Takes custody of sustained work.
    Warden,
}

/// The `[transport]` section: how the steward loads this plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Transport {
    /// Whether the plugin runs in-process or out-of-process.
    #[serde(rename = "type")]
    pub kind: TransportKind,
    /// Path to the artefact file, relative to the plugin directory.
    /// For example, `plugin.bin`, `plugin.so`, or `plugin.wasm`.
    pub exec: String,
}

/// Plugin transport kinds per `PLUGIN_CONTRACT.md` sections 7 and 8.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    /// Loaded into the steward process: compiled in, or cdylib.
    InProcess,
    /// Runs as a separate process; communicates over Unix socket.
    OutOfProcess,
}

/// The `[trust]` section: declared trust class.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Trust {
    /// Declared trust class. The steward may admit at a lower class if the
    /// signing key does not authorise the declared class.
    pub class: TrustClass,
}

/// Trust classes per `PLUGIN_PACKAGING.md` section 5.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum TrustClass {
    /// Highest class. In-process residency. System-wide custody.
    Platform,
    /// Separate process with elevated OS capabilities.
    Privileged,
    /// Separate process as the evo service user.
    Standard,
    /// Restricted user or namespace, no outbound network unless declared.
    Unprivileged,
    /// Sandbox (seccomp, namespace, or Wasm). No direct syscalls.
    Sandbox,
}

/// The `[prerequisites]` section: environmental requirements for admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prerequisites {
    /// Minimum evo workspace version.
    pub evo_min_version: Version,
    /// Required OS family: `linux` or `any`.
    #[serde(default = "default_os_family")]
    pub os_family: String,
    /// Does this plugin make outbound network calls?
    #[serde(default)]
    pub outbound_network: bool,
    /// Scoped filesystem paths needed. Empty means no filesystem access.
    #[serde(default)]
    pub filesystem_scopes: Vec<String>,
}

fn default_os_family() -> String {
    "linux".to_string()
}

/// The `[resources]` section: declared resource ceilings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resources {
    /// Maximum memory the plugin will use, in megabytes.
    pub max_memory_mb: u32,
    /// Maximum CPU share the plugin will use, expressed as a percentage.
    pub max_cpu_percent: u32,
}

/// The `[lifecycle]` section: hot-reload policy and restart behaviour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lifecycle {
    /// Hot-reload policy per `PLUGIN_CONTRACT.md` section 13.
    pub hot_reload: HotReloadPolicy,
    /// Whether the steward should start this plugin at steward startup.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// Whether the steward should restart this plugin after a crash.
    #[serde(default = "default_true")]
    pub restart_on_crash: bool,
    /// Maximum number of restarts permitted within a rolling one-hour window
    /// before the steward gives up and de-registers the plugin.
    #[serde(default = "default_restart_budget")]
    pub restart_budget: u32,
}

fn default_true() -> bool {
    true
}

fn default_restart_budget() -> u32 {
    5
}

/// Hot-reload policies per `PLUGIN_CONTRACT.md` section 13.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum HotReloadPolicy {
    /// Full unload-reload cycle on update.
    None,
    /// Process restart (or re-instantiation) on update without wider
    /// disruption.
    Restart,
    /// Plugin accepts a `reload_in_place` verb; custody retained across
    /// update.
    Live,
}

/// The `[capabilities]` section, with kind-specific sub-tables.
///
/// All sub-tables are optional at the serde layer; consistency with the
/// plugin's [`Kind`] is enforced by [`Capabilities::validate`] which is
/// called from [`Manifest::validate`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    /// Respondent-specific capabilities. Required iff
    /// `kind.interaction == Respondent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub respondent: Option<RespondentCapabilities>,

    /// Warden-specific capabilities. Required iff
    /// `kind.interaction == Warden`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warden: Option<WardenCapabilities>,

    /// Factory-specific capabilities. Required iff
    /// `kind.instance == Factory`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory: Option<FactoryCapabilities>,
}

impl Capabilities {
    /// Check that capability sub-tables are consistent with the plugin's
    /// declared [`Kind`].
    ///
    /// Rules:
    /// - `respondent` must be present iff `kind.interaction == Respondent`.
    /// - `warden` must be present iff `kind.interaction == Warden`.
    /// - `factory` must be present iff `kind.instance == Factory`.
    pub fn validate(&self, kind: &Kind) -> Result<(), ManifestError> {
        match kind.interaction {
            InteractionShape::Respondent => {
                if self.respondent.is_none() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.interaction is respondent but \
                         [capabilities.respondent] is missing"
                            .to_string(),
                    ));
                }
                if self.warden.is_some() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.interaction is respondent but \
                         [capabilities.warden] is present"
                            .to_string(),
                    ));
                }
            }
            InteractionShape::Warden => {
                if self.warden.is_none() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.interaction is warden but \
                         [capabilities.warden] is missing"
                            .to_string(),
                    ));
                }
                if self.respondent.is_some() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.interaction is warden but \
                         [capabilities.respondent] is present"
                            .to_string(),
                    ));
                }
            }
        }

        match kind.instance {
            InstanceShape::Factory => {
                if self.factory.is_none() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.instance is factory but \
                         [capabilities.factory] is missing"
                            .to_string(),
                    ));
                }
            }
            InstanceShape::Singleton => {
                if self.factory.is_some() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.instance is singleton but \
                         [capabilities.factory] is present"
                            .to_string(),
                    ));
                }
            }
        }

        Ok(())
    }
}

/// Respondent-specific capabilities per `PLUGIN_PACKAGING.md` section 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RespondentCapabilities {
    /// Shelf-shape-declared request types this plugin handles.
    pub request_types: Vec<String>,
    /// Deadline after which the steward declares a timeout.
    pub response_budget_ms: u32,
}

/// Warden-specific capabilities per `PLUGIN_PACKAGING.md` section 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WardenCapabilities {
    /// What this warden takes custody of, e.g. `playback`.
    pub custody_domain: String,
    /// Whether another warden of the same domain may coexist.
    pub custody_exclusive: bool,
    /// Deadline for fast-path course corrections.
    pub course_correction_budget_ms: u32,
    /// Behaviour when custody fails.
    pub custody_failure_mode: CustodyFailureMode,
}

/// Custody failure modes per `PLUGIN_PACKAGING.md` section 2.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CustodyFailureMode {
    /// Custody failure terminates the work under custody.
    Abort,
    /// Custody failure leaves partial results intact.
    PartialOk,
}

/// Factory-specific capabilities per `PLUGIN_PACKAGING.md` section 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FactoryCapabilities {
    /// Maximum number of concurrent instances the steward admits from this
    /// factory. Announcements beyond this are refused.
    pub max_instances: u32,
    /// Instance TTL in seconds. `0` means no TTL; instances live until
    /// retracted by the factory.
    pub instance_ttl_seconds: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_singleton_respondent() -> &'static str {
        r#"
[plugin]
name = "com.example.metadata.local"
version = "0.1.0"
contract = 1

[target]
shelf = "metadata.providers"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["metadata.query"]
response_budget_ms = 5000
"#
    }

    fn valid_factory_warden() -> &'static str {
        r#"
[plugin]
name = "com.example.sessions"
version = "1.2.3"
contract = 1

[target]
shelf = "sessions.active"
shape = 1

[kind]
instance = "factory"
interaction = "warden"

[transport]
type = "in-process"
exec = "plugin.so"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = true
filesystem_scopes = ["/var/lib/evo/plugins/com.example.sessions/state"]

[resources]
max_memory_mb = 128
max_cpu_percent = 10

[lifecycle]
hot_reload = "live"
autostart = true
restart_on_crash = true
restart_budget = 3

[capabilities.warden]
custody_domain = "session"
custody_exclusive = false
course_correction_budget_ms = 100
custody_failure_mode = "abort"

[capabilities.factory]
max_instances = 32
instance_ttl_seconds = 0
"#
    }

    #[test]
    fn parses_valid_singleton_respondent() {
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("valid manifest should parse");
        assert_eq!(m.plugin.name, "com.example.metadata.local");
        assert_eq!(m.plugin.contract, 1);
        assert_eq!(m.kind.instance, InstanceShape::Singleton);
        assert_eq!(m.kind.interaction, InteractionShape::Respondent);
        assert_eq!(m.transport.kind, TransportKind::OutOfProcess);
        assert_eq!(m.trust.class, TrustClass::Unprivileged);
        assert!(m.capabilities.respondent.is_some());
        assert!(m.capabilities.warden.is_none());
        assert!(m.capabilities.factory.is_none());
    }

    #[test]
    fn parses_valid_factory_warden() {
        let m = Manifest::from_toml(valid_factory_warden())
            .expect("valid factory-warden manifest should parse");
        assert_eq!(m.kind.instance, InstanceShape::Factory);
        assert_eq!(m.kind.interaction, InteractionShape::Warden);
        assert_eq!(m.transport.kind, TransportKind::InProcess);
        assert_eq!(m.trust.class, TrustClass::Platform);
        assert_eq!(m.lifecycle.hot_reload, HotReloadPolicy::Live);
        assert!(m.capabilities.warden.is_some());
        assert!(m.capabilities.factory.is_some());
        assert!(m.capabilities.respondent.is_none());
    }

    #[test]
    fn round_trip_singleton_respondent() {
        let m1 = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn round_trip_factory_warden() {
        let m1 = Manifest::from_toml(valid_factory_warden()).unwrap();
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn rejects_invalid_name_no_dot() {
        let toml = valid_singleton_respondent()
            .replace("com.example.metadata.local", "notanfqdn");
        match Manifest::from_toml(&toml) {
            Err(ManifestError::InvalidName(name)) => {
                assert_eq!(name, "notanfqdn");
            }
            other => panic!("expected InvalidName, got {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_name_uppercase() {
        let toml = valid_singleton_respondent()
            .replace("com.example.metadata.local", "Com.Example.Bad");
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InvalidName(_))
        ));
    }

    #[test]
    fn rejects_invalid_name_leading_digit() {
        let toml = valid_singleton_respondent()
            .replace("com.example.metadata.local", "1com.example.bad");
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InvalidName(_))
        ));
    }

    #[test]
    fn rejects_unsupported_contract_version() {
        let toml = valid_singleton_respondent().replace("contract = 1", "contract = 2");
        match Manifest::from_toml(&toml) {
            Err(ManifestError::UnsupportedContractVersion(v)) => assert_eq!(v, 2),
            other => panic!("expected UnsupportedContractVersion, got {other:?}"),
        }
    }

    #[test]
    fn rejects_warden_interaction_without_warden_capabilities() {
        let toml = valid_singleton_respondent()
            .replace(r#"interaction = "respondent""#, r#"interaction = "warden""#);
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InconsistentCapabilities(_))
        ));
    }

    #[test]
    fn rejects_singleton_with_factory_capabilities() {
        // Start from the factory-warden manifest and turn its instance back to
        // singleton without removing the factory capabilities section.
        let toml = valid_factory_warden().replace(
            r#"instance = "factory""#,
            r#"instance = "singleton""#,
        );
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InconsistentCapabilities(_))
        ));
    }

    #[test]
    fn rejects_factory_without_factory_capabilities() {
        let mut toml = valid_singleton_respondent().to_string();
        // Flip to factory without adding [capabilities.factory].
        toml = toml.replace(r#"instance = "singleton""#, r#"instance = "factory""#);
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InconsistentCapabilities(_))
        ));
    }

    #[test]
    fn rejects_missing_required_fields() {
        // Drop the [resources] table entirely.
        let toml = valid_singleton_respondent()
            .lines()
            .filter(|l| {
                !l.starts_with("[resources]")
                    && !l.starts_with("max_memory_mb")
                    && !l.starts_with("max_cpu_percent")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::ParseError(_))
        ));
    }

    #[test]
    fn trust_class_ordering() {
        // TrustClass derives Ord; the highest trust is Platform and the
        // lowest is Sandbox. Confirm the ordering matches the document.
        assert!(TrustClass::Platform < TrustClass::Privileged);
        assert!(TrustClass::Privileged < TrustClass::Standard);
        assert!(TrustClass::Standard < TrustClass::Unprivileged);
        assert!(TrustClass::Unprivileged < TrustClass::Sandbox);
    }

    #[test]
    fn instance_and_interaction_serialise_as_lowercase() {
        let toml_snippet = toml::to_string(&Kind {
            instance: InstanceShape::Factory,
            interaction: InteractionShape::Warden,
        })
        .unwrap();
        assert!(toml_snippet.contains(r#"instance = "factory""#));
        assert!(toml_snippet.contains(r#"interaction = "warden""#));
    }

    #[test]
    fn transport_kind_serialises_as_kebab_case() {
        let toml_snippet = toml::to_string(&Transport {
            kind: TransportKind::OutOfProcess,
            exec: "plugin.bin".to_string(),
        })
        .unwrap();
        assert!(toml_snippet.contains(r#"type = "out-of-process""#));
    }
}
