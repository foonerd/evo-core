//! Admission-time manifest-drift detection — re-export of the
//! SDK-side comparator plus framework-side tests.
//!
//! The drift comparator itself lives in
//! [`evo_plugin_sdk::drift`] so plugin authors can call the same
//! comparison logic from their unit tests via the
//! `evo-plugin-test` helper crate. This module exposes the same
//! types under the `evo::manifest_drift` path the framework's
//! admission code historically consumed; the implementations
//! delegate to the SDK.

pub use evo_plugin_sdk::drift::{detect_drift, DriftReport};

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::RuntimeCapabilities;
    use evo_plugin_sdk::manifest::Manifest;

    /// TOML fragment shared by every fixture in this module.
    /// `[plugin]`, `[target]`, `[transport]`, `[trust]`,
    /// `[prerequisites]`, `[resources]`, `[lifecycle]` — the
    /// per-fixture `[kind]` and `[capabilities.*]` are appended.
    const HEADER_SHARED: &str = r#"
[plugin]
name = "test.plugin"
version = "0.1.0"
contract = 1

[target]
shelf = "x.y"
shape = 1

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5
"#;

    fn manifest_respondent(types: Vec<&str>) -> Manifest {
        let toml = format!(
            r#"{HEADER_SHARED}
[kind]
instance = "singleton"
interaction = "respondent"

[capabilities.respondent]
request_types = [{}]
response_budget_ms = 100
"#,
            types
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Manifest::from_toml(&toml).expect("manifest must parse")
    }

    fn manifest_warden(verbs: Option<Vec<&str>>) -> Manifest {
        let verbs_field = match verbs {
            Some(v) => format!(
                "course_correct_verbs = [{}]",
                v.iter()
                    .map(|t| format!("\"{t}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => String::new(),
        };
        let toml = format!(
            r#"{HEADER_SHARED}
[kind]
instance = "singleton"
interaction = "warden"

[capabilities.warden]
custody_domain = "test"
custody_exclusive = false
course_correction_budget_ms = 100
custody_failure_mode = "abort"
{verbs_field}
"#,
        );
        Manifest::from_toml(&toml).expect("manifest must parse")
    }

    fn runtime_with_request_types(types: Vec<&str>) -> RuntimeCapabilities {
        RuntimeCapabilities {
            request_types: types.iter().map(|s| s.to_string()).collect(),
            ..RuntimeCapabilities::default()
        }
    }

    fn runtime_with_warden_verbs(verbs: Vec<&str>) -> RuntimeCapabilities {
        RuntimeCapabilities {
            course_correct_verbs: verbs.iter().map(|s| s.to_string()).collect(),
            ..RuntimeCapabilities::default()
        }
    }

    #[test]
    fn no_drift_when_respondent_sets_match() {
        let m = manifest_respondent(vec!["a", "b"]);
        let r = runtime_with_request_types(vec!["a", "b"]);
        assert!(detect_drift(&m, &r).is_empty());
    }

    #[test]
    fn drift_when_manifest_advertises_more_than_implementation() {
        let m = manifest_respondent(vec!["a", "b", "c"]);
        let r = runtime_with_request_types(vec!["a"]);
        let report = detect_drift(&m, &r);
        assert_eq!(
            report.missing_in_implementation,
            vec!["b".to_string(), "c".to_string()]
        );
        assert!(report.missing_in_manifest.is_empty());
    }

    #[test]
    fn drift_when_implementation_provides_undeclared_verb() {
        let m = manifest_respondent(vec!["a"]);
        let r = runtime_with_request_types(vec!["a", "extra"]);
        let report = detect_drift(&m, &r);
        assert!(report.missing_in_implementation.is_empty());
        assert_eq!(report.missing_in_manifest, vec!["extra".to_string()]);
    }

    #[test]
    fn no_drift_when_warden_set_matches() {
        let m = manifest_warden(Some(vec!["pause", "resume"]));
        let r = runtime_with_warden_verbs(vec!["pause", "resume"]);
        assert!(detect_drift(&m, &r).is_empty());
    }

    #[test]
    fn drift_when_warden_manifest_advertises_more() {
        let m = manifest_warden(Some(vec!["pause", "resume", "set_volume"]));
        let r = runtime_with_warden_verbs(vec!["pause", "resume"]);
        let report = detect_drift(&m, &r);
        assert_eq!(
            report.missing_in_implementation,
            vec!["set_volume".to_string()]
        );
    }

    #[test]
    fn drift_check_skips_warden_when_manifest_omits_verbs() {
        // Legacy plugin: manifest's course_correct_verbs is None.
        // Drift detection skips the warden comparison entirely;
        // runtime can carry any verb list without producing
        // drift. The version-skew policy decides whether to
        // refuse the plugin for failing to declare the field.
        let m = manifest_warden(None);
        let r = runtime_with_warden_verbs(vec!["anything"]);
        assert!(detect_drift(&m, &r).is_empty());
    }
}
