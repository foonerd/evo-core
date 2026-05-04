//! Semantic validator for privileges records.
//!
//! Structural validation (required fields, types, patterns, enums,
//! minItems / minLength) is performed by the embedded JSON Schema at
//! parse time — see [`super::types::PrivilegesV1::from_yaml`] which
//! delegates to the `jsonschema` crate against
//! `schemas/privileges.v1.json`. The validator here carries ONLY what
//! JSON Schema cannot express:
//!
//! - **Cross-field rules.** AmbientCapabilities must be a subset of
//!   CapabilityBoundingSet. capability_intent ids must be unique
//!   within a record. required_binaries names must be unique.
//! - **Best-practice advisories.** Intent-only records (oop +
//!   no host_provisioning) surface as `intent_only_record` (Warn).
//!   `User=` absent surfaces as `user_resolved_at_install_time`
//!   (Warn). Non-empty extra `capabilities` or `sudoers` arrays
//!   surface as audit-flag advisories. `NoNewPrivileges=false`
//!   surfaces as advisory requiring explicit justification.
//!
//! All findings are tagged with [`ValidationSeverity::Block`]
//! (admission-blocking) or [`ValidationSeverity::Warn`] (advisory;
//! surfaces in audit reports). The schema's own violations land in
//! [`super::types::PrivilegesError::SchemaValidation`] before the
//! validator runs at all.

use std::collections::HashSet;
use std::fmt;

use super::types::{
    HostProvisioningBlock, Isolation, PrivilegesV1, SystemdProvisioning,
};

/// Severity of a validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationSeverity {
    /// Issue blocks admission. The framework refuses to admit a surface
    /// whose record carries any `Block` issue.
    Block,
    /// Advisory. Logged at audit; does not block admission.
    Warn,
}

/// One validation finding.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    /// Severity classification.
    pub severity: ValidationSeverity,
    /// Stable identifier (lowercase_snake_case) so callers can pattern-
    /// match without relying on prose.
    pub code: &'static str,
    /// Human-readable description with the offending value embedded.
    pub message: String,
}

/// Collection of validation findings; an `Err` from [`PrivilegesV1::validate`].
#[derive(Debug)]
pub struct ValidationError {
    /// All findings, blocking and advisory.
    pub issues: Vec<ValidationIssue>,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} blocking issue(s), {} warning(s)",
            self.blocking_count(),
            self.warning_count()
        )
    }
}

impl std::error::Error for ValidationError {}

impl ValidationError {
    /// Count of issues with severity `Block`.
    pub fn blocking_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == ValidationSeverity::Block)
            .count()
    }

    /// Count of issues with severity `Warn`.
    pub fn warning_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == ValidationSeverity::Warn)
            .count()
    }

    /// True if any issue is blocking.
    pub fn has_blocking(&self) -> bool {
        self.blocking_count() > 0
    }
}

impl PrivilegesV1 {
    /// Run the semantic validator. Returns the full set of findings or
    /// `Ok(())` if no blocking issues. Warnings are exposed even on
    /// success via [`PrivilegesV1::validate_collect`].
    pub fn validate(&self) -> Result<(), ValidationError> {
        let issues = self.validate_collect();
        if issues
            .iter()
            .any(|i| i.severity == ValidationSeverity::Block)
        {
            Err(ValidationError { issues })
        } else {
            Ok(())
        }
    }

    /// Run the semantic validator and return all issues (blocking +
    /// advisory). Used by tooling that surfaces warnings without
    /// failing.
    pub fn validate_collect(&self) -> Vec<ValidationIssue> {
        // Per LOGGING.md §2 (each verb invocation fires at debug):
        // privileges validation is a verb-shaped operation called
        // once per plugin admission against a record's privilege
        // surface.
        tracing::debug!(
            isolation = ?self.isolation,
            capability_intents = self.capability_intent.len(),
            required_binaries = self.required_binaries.len(),
            host_provisioning = self.host_provisioning.0.len(),
            "privileges validator: validate invoking"
        );
        let mut issues = Vec::new();

        // Cross-field rule: capability_intent ids unique within record.
        let mut seen_intent_ids: HashSet<&str> = HashSet::new();
        for intent in &self.capability_intent {
            if !seen_intent_ids.insert(intent.id.as_str()) {
                issues.push(ValidationIssue {
                    severity: ValidationSeverity::Block,
                    code: "intent_id_duplicate",
                    message: format!(
                        "capability_intent id `{}` is declared more than once",
                        intent.id
                    ),
                });
            }
        }

        // Cross-field advisory: required_binaries names unique.
        let mut seen_binaries: HashSet<&str> = HashSet::new();
        for bin in &self.required_binaries {
            if !seen_binaries.insert(bin.name.as_str()) {
                issues.push(ValidationIssue {
                    severity: ValidationSeverity::Warn,
                    code: "binary_name_duplicate",
                    message: format!(
                        "required_binary `{}` is declared more than once",
                        bin.name
                    ),
                });
            }
        }

        // Cross-field advisory: framework-tier records may legitimately
        // ship intent-only (no host_provisioning). The advisory
        // documents the deferral so a future reader sees it is
        // intentional, not an oversight.
        if self.isolation == Isolation::Oop && self.host_provisioning.is_empty()
        {
            issues.push(ValidationIssue {
                severity: ValidationSeverity::Warn,
                code: "intent_only_record",
                message:
                    "isolation=oop with no host_provisioning blocks; intent-only record (downstream distribution must provide provisioning)"
                        .to_string(),
            });
        }

        // Per-distribution cross-field rules + advisories.
        for distribution in self.host_provisioning.distributions() {
            if let Some(block) = self.host_provisioning.get(distribution) {
                validate_block_semantics(distribution, block, &mut issues);
            }
        }

        issues
    }
}

fn validate_block_semantics(
    distribution: &str,
    block: &HostProvisioningBlock,
    issues: &mut Vec<ValidationIssue>,
) {
    validate_systemd_semantics(distribution, &block.systemd, issues);
    if !block.capabilities.is_empty() {
        issues.push(ValidationIssue {
            severity: ValidationSeverity::Warn,
            code: "extra_capabilities_present",
            message: format!(
                "host_provisioning[{}].capabilities is non-empty ({} entries); audit-flag for justification — prefer CapabilityBoundingSet/AmbientCapabilities under systemd",
                distribution,
                block.capabilities.len()
            ),
        });
    }
    if !block.sudoers.is_empty() {
        issues.push(ValidationIssue {
            severity: ValidationSeverity::Warn,
            code: "sudoers_present",
            message: format!(
                "host_provisioning[{}].sudoers is non-empty ({} entries); audit-flag — sudoers is operator-CLI only, never on plugin runtime paths",
                distribution,
                block.sudoers.len()
            ),
        });
    }
}

fn validate_systemd_semantics(
    distribution: &str,
    systemd: &SystemdProvisioning,
    issues: &mut Vec<ValidationIssue>,
) {
    // Cross-field advisory: User absent → installer resolves at
    // install time. Schema permits absence; this advisory documents
    // the deferral.
    if systemd.user.is_none() {
        issues.push(ValidationIssue {
            severity: ValidationSeverity::Warn,
            code: "user_resolved_at_install_time",
            message: format!(
                "host_provisioning[{}].systemd.User absent; installer resolves at install time per distribution policy (appliance-class default: operator's first user at uid >= 1000). Vendor distributions pre-baking a service user should pin User explicitly.",
                distribution
            ),
        });
    }

    // Cross-field rule: AmbientCapabilities ⊆ CapabilityBoundingSet.
    let bounding: HashSet<&str> = systemd
        .capability_bounding_set
        .iter()
        .map(String::as_str)
        .collect();
    for cap in &systemd.ambient_capabilities {
        if !bounding.contains(cap.as_str()) {
            issues.push(ValidationIssue {
                severity: ValidationSeverity::Block,
                code: "ambient_not_in_bounding",
                message: format!(
                    "host_provisioning[{}].systemd.AmbientCapabilities entry `{}` is not in CapabilityBoundingSet — ambient must be a subset of the bounding set",
                    distribution, cap
                ),
            });
        }
    }

    // Cross-field advisory: NoNewPrivileges=false requires explicit
    // justification at audit.
    if matches!(systemd.no_new_privileges, Some(false)) {
        issues.push(ValidationIssue {
            severity: ValidationSeverity::Warn,
            code: "no_new_privileges_disabled",
            message: format!(
                "host_provisioning[{}].systemd.NoNewPrivileges=false requires explicit justification at audit",
                distribution
            ),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::PrivilegesError;
    use super::*;

    fn sample_record() -> &'static str {
        r#"
schema_version: "1.0"
plugin: org.evoframework.network.nm
owner: networking
isolation: oop
capability_intent:
  - id: nm_control
    need: control NetworkManager connections
    access_path: dbus/polkit
    failure_mode: degraded status; no-panic
required_binaries:
  - name: nmcli
    min_version: "1.30"
    failure_mode: refuse to start
required_kernel_modules: []
required_system_services:
  - NetworkManager.service
verification:
  commands:
    - nmcli general status
  expected:
    - command exits 0 as service identity
host_provisioning:
  debian:
    systemd:
      User: dynamic
      SupplementaryGroups: [netdev]
      CapabilityBoundingSet: []
      AmbientCapabilities: []
      ProtectSystem: strict
    polkit:
      policy_file: 50-evo-org.evoframework.network.nm.rules
      required_actions:
        - org.freedesktop.NetworkManager.*
    capabilities: []
    sudoers: []
"#
    }

    #[test]
    fn sample_record_round_trips() {
        let record = PrivilegesV1::from_yaml(sample_record()).unwrap();
        assert_eq!(record.plugin, "org.evoframework.network.nm");
        assert_eq!(record.isolation, Isolation::Oop);
        assert_eq!(record.required_binaries.len(), 1);
        assert!(record.host_provisioning.get("debian").is_some());
        let yaml = record.to_yaml().unwrap();
        let reparsed = PrivilegesV1::from_yaml(&yaml).unwrap();
        assert_eq!(reparsed.plugin, record.plugin);
    }

    #[test]
    fn sample_record_validates_clean() {
        let record = PrivilegesV1::from_yaml(sample_record()).unwrap();
        record.validate().expect("sample should validate");
    }

    #[test]
    fn invalid_plugin_id_is_caught_by_schema_at_parse() {
        let bad =
            sample_record().replace("org.evoframework.network.nm", "BadName");
        let err = PrivilegesV1::from_yaml(&bad).unwrap_err();
        match err {
            PrivilegesError::SchemaValidation(violations) => {
                assert!(
                    violations
                        .iter()
                        .any(|v| v.instance_path.contains("plugin")
                            && v.keyword.contains("pattern")),
                    "expected pattern violation on plugin path; got {violations:?}"
                );
            }
            other => panic!("expected SchemaValidation, got {other:?}"),
        }
    }

    #[test]
    fn ambient_outside_bounding_blocks_at_validator() {
        let bad = sample_record().replace(
            "AmbientCapabilities: []",
            "AmbientCapabilities: [CAP_NET_ADMIN]",
        );
        let record = PrivilegesV1::from_yaml(&bad).unwrap();
        let err = record.validate().unwrap_err();
        assert!(err
            .issues
            .iter()
            .any(|i| i.code == "ambient_not_in_bounding"));
    }

    #[test]
    fn intent_only_oop_record_warns_not_blocks() {
        let intent_only = r#"
schema_version: "1.0"
plugin: org.evoframework.steward
owner: evo-framework-core
isolation: oop
capability_intent: []
required_binaries: []
required_kernel_modules: []
required_system_services: []
verification:
  commands: ["true"]
  expected: ["always passes"]
host_provisioning: {}
"#;
        let record = PrivilegesV1::from_yaml(intent_only).unwrap();
        record
            .validate()
            .expect("intent-only record validates without blocking issues");
        let issues = record.validate_collect();
        assert!(issues.iter().any(|i| i.code == "intent_only_record"
            && i.severity == ValidationSeverity::Warn));
    }

    #[test]
    fn user_absent_warns_with_install_time_advisory() {
        let no_user = r#"
schema_version: "1.0"
plugin: org.evoframework.example
owner: example
isolation: oop
capability_intent: []
required_binaries: []
required_kernel_modules: []
required_system_services: []
verification:
  commands: ["true"]
  expected: ["always passes"]
host_provisioning:
  debian:
    systemd:
      SupplementaryGroups: []
      CapabilityBoundingSet: []
      AmbientCapabilities: []
    capabilities: []
    sudoers: []
"#;
        let record = PrivilegesV1::from_yaml(no_user).unwrap();
        record.validate().expect("user-absent record validates");
        let issues = record.validate_collect();
        assert!(issues
            .iter()
            .any(|i| i.code == "user_resolved_at_install_time"
                && i.severity == ValidationSeverity::Warn));
    }

    #[test]
    fn duplicate_intent_ids_blocked_at_validator() {
        let bad = r#"
schema_version: "1.0"
plugin: org.evoframework.example
owner: example
isolation: oop
capability_intent:
  - id: foo
    need: first
    access_path: filesystem/state
    failure_mode: degrade
  - id: foo
    need: second with same id
    access_path: filesystem/state
    failure_mode: degrade
required_binaries: []
required_kernel_modules: []
required_system_services: []
verification:
  commands: ["true"]
  expected: ["always passes"]
host_provisioning: {}
"#;
        let record = PrivilegesV1::from_yaml(bad).unwrap();
        let err = record.validate().unwrap_err();
        assert!(err.issues.iter().any(|i| i.code == "intent_id_duplicate"));
    }

    #[test]
    fn unknown_field_caught_by_schema() {
        let bad = sample_record()
            .replace("isolation: oop", "isolation: oop\nrogue_field: hi");
        let err = PrivilegesV1::from_yaml(&bad).unwrap_err();
        match err {
            PrivilegesError::SchemaValidation(violations) => {
                assert!(
                    violations.iter().any(|v| v.keyword == "additionalProperties"
                        || v.message.contains("rogue_field")),
                    "expected additionalProperties violation; got {violations:?}"
                );
            }
            other => panic!("expected SchemaValidation, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_schema_version_caught_by_schema() {
        let bad = sample_record().replace("\"1.0\"", "\"2.0\"");
        let err = PrivilegesV1::from_yaml(&bad).unwrap_err();
        match err {
            PrivilegesError::SchemaValidation(violations) => {
                assert!(
                    violations.iter().any(|v| v.keyword == "const"
                        && v.instance_path.contains("schema_version")),
                    "expected const violation on schema_version; got {violations:?}"
                );
            }
            other => panic!("expected SchemaValidation, got {other:?}"),
        }
    }

    #[test]
    fn warning_for_non_empty_sudoers() {
        let bad = sample_record().replace(
            "sudoers: []",
            "sudoers: [\"steward ALL=(ALL) NOPASSWD: /bin/true\"]",
        );
        let record = PrivilegesV1::from_yaml(&bad).unwrap();
        record.validate().expect("warning-only record validates");
        let issues = record.validate_collect();
        assert!(issues.iter().any(|i| i.code == "sudoers_present"
            && i.severity == ValidationSeverity::Warn));
    }
}
