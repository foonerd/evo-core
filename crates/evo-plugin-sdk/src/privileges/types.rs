//! Rust types modelling the privileges contract v1.0.
//!
//! Types match `schemas/privileges.v1.json` field-for-field. Serde
//! deserialisation uses `deny_unknown_fields` so unknown keys at any level
//! are a parse error — the schema is closed; new fields require a schema
//! version bump.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A complete privileges record for one surface (plugin / steward /
/// distribution-bootstrap). Loaded from a `privileges.yaml` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivilegesV1 {
    /// Schema version. Must equal `"1.0"` for this type to apply.
    pub schema_version: String,

    /// Namespaced plugin / surface identifier (e.g.
    /// `org.evoframework.network.nm`).
    pub plugin: String,

    /// Team / individual / organisation accountable for this surface.
    pub owner: String,

    /// How this surface runs relative to the steward identity.
    pub isolation: Isolation,

    /// What the surface needs to do, in domain terms.
    pub capability_intent: Vec<CapabilityIntent>,

    /// External executables the surface invokes at runtime.
    pub required_binaries: Vec<RequiredBinary>,

    /// Kernel modules that must be loaded (or loadable).
    pub required_kernel_modules: Vec<String>,

    /// systemd unit names the surface depends on (e.g. `mpd.service`).
    pub required_system_services: Vec<String>,

    /// Read-only commands that confirm the contract is satisfied.
    pub verification: Verification,

    /// Per-distribution OS-level provisioning. Keyed by lowercase
    /// distribution name.
    pub host_provisioning: HostProvisioning,
}

/// How a surface runs relative to the steward identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Out-of-process: its own systemd unit with isolated identity.
    Oop,
    /// In-process: loaded into the steward; privileges union into
    /// the steward identity (audited as the higher-trust class).
    InProcess,
}

/// One independently-testable capability the surface needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityIntent {
    /// Stable identifier within the surface (lowercase_snake_case).
    pub id: String,
    /// Human-readable description of the need.
    pub need: String,
    /// Mechanism class (e.g. `dbus/polkit`, `filesystem/state`).
    pub access_path: String,
    /// What happens when permission is absent at runtime. Must be
    /// non-panic and observable.
    pub failure_mode: String,
}

/// An external executable the surface invokes at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredBinary {
    /// Binary name as it appears on PATH (no path components).
    pub name: String,
    /// Optional minimum acceptable version, semver-shaped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    /// What happens when the binary is missing or below min_version.
    pub failure_mode: String,
}

/// Read-only verification commands run as the surface's declared
/// service identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    /// Shell commands. Each must be read-only and exit 0 on success.
    pub commands: Vec<String>,
    /// Human-readable expectations the commands collectively confirm.
    pub expected: Vec<String>,
}

/// Per-distribution host provisioning. Keyed by lowercase distribution
/// name (`debian`, `fedora`, `alpine`, ...). Required for
/// `Isolation::Oop`; optional/advisory for `Isolation::InProcess`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostProvisioning(pub BTreeMap<String, HostProvisioningBlock>);

impl HostProvisioning {
    /// True if no distribution blocks declared.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Look up the block for a given distribution.
    pub fn get(&self, distribution: &str) -> Option<&HostProvisioningBlock> {
        self.0.get(distribution)
    }

    /// Distribution names declared, in sorted order.
    pub fn distributions(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

/// One distribution's provisioning declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProvisioningBlock {
    /// systemd unit identity + sandboxing.
    pub systemd: SystemdProvisioning,
    /// Optional polkit drop-in (omit if no polkit-mediated actions
    /// are needed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polkit: Option<PolkitProvisioning>,
    /// Additional Linux capabilities outside CapabilityBoundingSet.
    /// Should be empty; non-empty entries flagged at audit.
    pub capabilities: Vec<String>,
    /// Explicit sudoers entries. Should be empty for runtime plugin
    /// paths; non-empty entries flagged at audit.
    pub sudoers: Vec<String>,
}

/// systemd unit identity and sandboxing for one distribution.
///
/// Field names are PascalCase to match systemd unit-file convention
/// (so the Rust struct can be serialised directly into a unit file).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct SystemdProvisioning {
    /// Service identity. Optional. When present, `dynamic` selects
    /// DynamicUser=yes; a named user persists state across restarts.
    /// When absent, the installer resolves at install time per the
    /// distribution's policy (typically the operator's first user at
    /// uid >= 1000). The appliance-class default is to omit User.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Supplementary groups granted to the service identity.
    pub supplementary_groups: Vec<String>,
    /// Linux capabilities the unit MAY receive (upper bound).
    pub capability_bounding_set: Vec<String>,
    /// Linux capabilities the unit starts with (subset of the
    /// bounding set).
    pub ambient_capabilities: Vec<String>,
    /// systemd ProtectSystem= value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protect_system: Option<String>,
    /// systemd ProtectHome= value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protect_home: Option<String>,
    /// Filesystem paths the unit may read/write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_write_paths: Vec<String>,
    /// Filesystem paths the unit may read but not write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only_paths: Vec<String>,
    /// systemd NoNewPrivileges= value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_new_privileges: Option<bool>,
}

/// polkit drop-in declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolkitProvisioning {
    /// Drop-in filename (placed in /etc/polkit-1/rules.d/ by the
    /// distribution-provided unit-installer). Convention:
    /// `50-evo-<plugin-id>.rules`.
    pub policy_file: String,
    /// polkit action identifiers the drop-in authorises.
    pub required_actions: Vec<String>,
}

/// Errors that can occur when parsing a privileges record from YAML.
#[derive(Debug, Error)]
pub enum PrivilegesError {
    /// YAML parse failed (malformed YAML, type mismatch).
    #[error("YAML parse error: {0}")]
    YamlParse(#[from] serde_yaml::Error),

    /// JSON conversion failed (between YAML value and JSON value used
    /// for schema validation, or between JSON value and the typed
    /// struct).
    #[error("JSON conversion error: {0}")]
    JsonConvert(#[from] serde_json::Error),

    /// Record fails the embedded JSON Schema. One entry per violation
    /// in the order the validator yielded them.
    #[error("schema validation failed: {} violation(s)", .0.len())]
    SchemaValidation(Vec<SchemaViolation>),
}

/// One schema-validation violation extracted from the `jsonschema`
/// crate's `ValidationError`. Stable shape so callers can pattern-
/// match on `keyword` instead of parsing the prose `message`.
#[derive(Debug, Clone)]
pub struct SchemaViolation {
    /// JSON Schema keyword that failed (`"required"`, `"pattern"`,
    /// `"const"`, `"enum"`, `"additionalProperties"`, etc.).
    pub keyword: String,
    /// Instance path within the document (e.g. `/host_provisioning/debian/systemd/User`).
    pub instance_path: String,
    /// Schema path that holds the failing constraint.
    pub schema_path: String,
    /// Human-readable message from the jsonschema crate.
    pub message: String,
}

impl PrivilegesV1 {
    /// Parse a YAML string into a privileges record.
    ///
    /// Validates the document against the embedded JSON Schema
    /// (`schemas/privileges.v1.json`) BEFORE typed deserialisation,
    /// so structural violations surface as
    /// [`PrivilegesError::SchemaValidation`] with stable
    /// keyword + instance_path tuples. Cross-field semantic checks
    /// (ambient ⊆ bounding, duplicate ids, advisories) live in the
    /// validator module — call [`PrivilegesV1::validate`] after
    /// successful parse.
    pub fn from_yaml(yaml: &str) -> Result<Self, PrivilegesError> {
        // YAML → serde_yaml::Value → serde_json::Value so the
        // jsonschema crate (which works on JSON Values) can validate.
        let yaml_value: serde_yaml::Value = serde_yaml::from_str(yaml)?;
        let json_value: serde_json::Value = serde_json::to_value(&yaml_value)?;

        // Compile and validate against the embedded schema bytes.
        // The schema is ~270 lines of JSON; compile cost is negligible
        // and not on a hot path. Draft is auto-detected from the
        // schema's $schema field.
        let schema_value: serde_json::Value =
            serde_json::from_slice(super::SCHEMA_V1_BYTES)?;
        let compiled = jsonschema::JSONSchema::compile(&schema_value).expect(
            "embedded privileges.v1.json is invalid; this is a build-time bug",
        );

        if let Err(errors) = compiled.validate(&json_value) {
            let violations: Vec<SchemaViolation> = errors
                .map(|e| SchemaViolation {
                    keyword: schema_violation_keyword(&e.kind),
                    instance_path: e.instance_path.to_string(),
                    schema_path: e.schema_path.to_string(),
                    message: e.to_string(),
                })
                .collect();
            return Err(PrivilegesError::SchemaValidation(violations));
        }

        // Schema passed; typed deserialisation cannot fail unless the
        // schema and the Rust types diverge (a build-time bug). Any
        // residual serde error surfaces as PrivilegesError::JsonConvert.
        let record: PrivilegesV1 = serde_json::from_value(json_value)?;
        Ok(record)
    }

    /// Serialise back to YAML. Round-trips with [`PrivilegesV1::from_yaml`].
    pub fn to_yaml(&self) -> Result<String, PrivilegesError> {
        Ok(serde_yaml::to_string(self)?)
    }
}

/// Map a `jsonschema::ValidationErrorKind` to a stable lowercase keyword
/// string (`"required"`, `"pattern"`, `"const"`, `"enum"`,
/// `"additionalProperties"`, `"type"`, `"minItems"`, `"minLength"`,
/// etc.) that callers can pattern-match on without parsing prose.
/// The mapping is exhaustive against jsonschema 0.18.3's
/// `ValidationErrorKind` variants; any future variant added by the
/// crate will surface via the catch-all and be picked up at the next
/// crate bump (caught by clippy's wildcard-arm warning if we ever
/// reach for `match` on this kind in another path).
fn schema_violation_keyword(
    kind: &jsonschema::error::ValidationErrorKind,
) -> String {
    use jsonschema::error::ValidationErrorKind as K;
    match kind {
        K::AdditionalItems { .. } => "additionalItems",
        K::AdditionalProperties { .. } => "additionalProperties",
        K::AnyOf => "anyOf",
        K::BacktrackLimitExceeded { .. } => "backtrackLimitExceeded",
        K::Constant { .. } => "const",
        K::Contains => "contains",
        K::ContentEncoding { .. } => "contentEncoding",
        K::ContentMediaType { .. } => "contentMediaType",
        K::Custom { .. } => "custom",
        K::Enum { .. } => "enum",
        K::ExclusiveMaximum { .. } => "exclusiveMaximum",
        K::ExclusiveMinimum { .. } => "exclusiveMinimum",
        K::FalseSchema => "falseSchema",
        K::FileNotFound { .. } => "fileNotFound",
        K::Format { .. } => "format",
        K::JSONParse { .. } => "jsonParse",
        K::InvalidReference { .. } => "invalidReference",
        K::InvalidURL { .. } => "invalidURL",
        K::MaxItems { .. } => "maxItems",
        K::Maximum { .. } => "maximum",
        K::MaxLength { .. } => "maxLength",
        K::MaxProperties { .. } => "maxProperties",
        K::MinItems { .. } => "minItems",
        K::Minimum { .. } => "minimum",
        K::MinLength { .. } => "minLength",
        K::MinProperties { .. } => "minProperties",
        K::MultipleOf { .. } => "multipleOf",
        K::Not { .. } => "not",
        K::OneOfMultipleValid => "oneOfMultipleValid",
        K::OneOfNotValid => "oneOfNotValid",
        K::Pattern { .. } => "pattern",
        K::PropertyNames { .. } => "propertyNames",
        K::Required { .. } => "required",
        K::Resolver { .. } => "resolver",
        K::Schema => "schema",
        K::Type { .. } => "type",
        K::UnevaluatedProperties { .. } => "unevaluatedProperties",
        K::UniqueItems => "uniqueItems",
        K::UnknownReferenceScheme { .. } => "unknownReferenceScheme",
        _ => "unknown",
    }
    .to_string()
}
