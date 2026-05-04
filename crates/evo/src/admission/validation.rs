//! Pre-admission manifest validation.
//!
//! Two checks every admit path runs before constructing a
//! [`LoadContext`](evo_plugin_sdk::contract::LoadContext) or
//! spawning a child process:
//!
//! - [`check_manifest_prerequisites`]: the manifest's declared
//!   prerequisites (evo version, host OS) match the running build.
//! - [`check_admin_trust`]: a plugin declaring
//!   `capabilities.admin = true` carries an effective trust class at
//!   or above [`evo_trust::ADMIN_MINIMUM_TRUST`].
//!
//! The four `admit_*` entry points in the parent module call all
//! three in the same order. Centralising them here gives each check
//! one canonical site, makes "what does admission reject"
//! answerable from one file, and keeps the pipeline mechanically
//! auditable in code review.

use std::sync::Arc;
use std::time::SystemTime;

use crate::error::StewardError;
use crate::happenings::{Happening, HappeningBus};
use crate::manifest_drift::{detect_drift, DriftReport};
use crate::version_skew::{
    classify_skew, skew_minor_versions, SkewClassification,
};
use evo_plugin_sdk::contract::RuntimeCapabilities;
use evo_plugin_sdk::Manifest;
use evo_trust::ADMIN_MINIMUM_TRUST;

/// Synthetic-acceptance-only override for the framework version
/// used by `check_manifest_prerequisites` /
/// `check_drift_and_skew`. When set to a parseable semver, both
/// helpers behave as if the steward were that version; when unset
/// or unparseable, the canonical `CARGO_PKG_VERSION` is used.
///
/// Required by `T2.version-skew-warn-band`: the WarnBand band is
/// `framework_minor - plugin_minor == 2`. With the framework's
/// own minor version at 1, no real plugin can land in WarnBand
/// (would need `plugin_minor = -1`), so the test sets this
/// override to a value like `0.3.0` and admits a plugin with
/// `evo_min_version = 0.1.0` to exercise the WarnBand emit path.
///
/// Production stewards never set this env. The acceptance
/// distribution sets it from a systemd drop-in inside the test's
/// trap-bounded scope and unsets it on cleanup.
const ENV_FRAMEWORK_VERSION_OVERRIDE: &str = "EVO_FRAMEWORK_VERSION_OVERRIDE";

/// Resolve the framework version the admission validators should
/// compare against. Reads the
/// [`ENV_FRAMEWORK_VERSION_OVERRIDE`] env when set; falls back to
/// the compiled-in `CARGO_PKG_VERSION` otherwise. An unparseable
/// override returns the canonical version (logged at warn) so a
/// typo never silently disables version-skew enforcement.
fn framework_version() -> Result<semver::Version, StewardError> {
    if let Ok(raw) = std::env::var(ENV_FRAMEWORK_VERSION_OVERRIDE) {
        match semver::Version::parse(&raw) {
            Ok(v) => return Ok(v),
            Err(e) => {
                tracing::warn!(
                    env = ENV_FRAMEWORK_VERSION_OVERRIDE,
                    raw = %raw,
                    error = %e,
                    "framework version override is set but unparseable; \
                     falling back to compiled-in CARGO_PKG_VERSION"
                );
            }
        }
    }
    semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(|e| {
        StewardError::Admission(format!(
            "evo's own CARGO_PKG_VERSION is not valid semver: {e}"
        ))
    })
}

/// Enforce the in-scope half of `[prerequisites]` at admission.
///
/// Called from every `admit_*` entry point after
/// `manifest.validate()`. Checks:
/// - `evo_min_version` against the evo steward's own version
///   (compiled in via `env!("CARGO_PKG_VERSION")`).
/// - `os_family` against the host OS (`std::env::consts::OS`). The
///   special value `"any"` always matches.
///
/// The remaining `[prerequisites]` and `[resources]` fields
/// (`outbound_network`, `filesystem_scopes`, `max_memory_mb`,
/// `max_cpu_percent`) are out of scope for core: they require
/// distribution-owned machinery (cgroups, network namespaces,
/// bind mounts). Those fields remain in the manifest so
/// distributions can enforce them via systemd / image policy. See
/// `PLUGIN_PACKAGING.md` section 2 ("Enforcement scope") for the
/// split.
///
/// Returns `StewardError::Admission` rather than panicking when
/// `CARGO_PKG_VERSION` itself fails to parse so a bizarre crate
/// version (e.g. `"0.1.8+dirty"` in a fork) gives a runnable
/// error surface. In practice the workspace pins
/// `version.workspace = true` to a clean semver.
pub(super) fn check_manifest_prerequisites(
    manifest: &Manifest,
) -> Result<(), StewardError> {
    let evo_version = framework_version()?;
    manifest.check_prerequisites(&evo_version, std::env::consts::OS)?;
    Ok(())
}

/// Enforce the admin-trust gate.
///
/// Called from every admit entry point AFTER
/// [`check_manifest_prerequisites`] and any trust-degradation pass
/// (for on-disk out-of-process admission) so the manifest's
/// `trust.class` reflects the effective class, not just the
/// declared one.
///
/// A plugin that declares `capabilities.admin = true` is refused
/// with [`StewardError::AdminTrustTooLow`] when its effective
/// class is above
/// [`evo_trust::ADMIN_MINIMUM_TRUST`]. Recall that on
/// [`TrustClass`](evo_plugin_sdk::manifest::TrustClass) lower
/// ordinal = more privileged: `Platform` (0) is strictly more
/// privileged than `Privileged` (1) which is strictly more
/// privileged than `Standard` (2), and so on. `Platform` and
/// `Privileged` qualify; `Standard` and below do not.
///
/// Plugins with `capabilities.admin = false` (the default) bypass
/// this check entirely.
pub(super) fn check_admin_trust(
    manifest: &Manifest,
) -> Result<(), StewardError> {
    if !manifest.capabilities.admin {
        return Ok(());
    }
    let effective = manifest.trust.class;
    if effective > ADMIN_MINIMUM_TRUST {
        return Err(StewardError::AdminTrustTooLow {
            plugin_name: manifest.plugin.name.clone(),
            effective,
            minimum: ADMIN_MINIMUM_TRUST,
        });
    }
    Ok(())
}

/// Run the version-skew classification + manifest-drift check at
/// admission, emitting structured happenings for warn-band
/// admissions and refusing admission for out-of-window or
/// strict-window-with-drift cases.
///
/// Called from every admit entry point AFTER
/// [`check_manifest_prerequisites`] and after
/// `plugin.describe()` returns. The function:
///
/// 1. Classifies the plugin's `prerequisites.evo_min_version`
///    against the framework's running version.
/// 2. Refuses admission outright on `OutOfWindow`. (`TooNew` was
///    already refused by `check_manifest_prerequisites`.)
/// 3. Computes a [`DriftReport`] comparing the manifest's
///    declared verb sets to the runtime `describe()` response.
/// 4. Refuses admission on Strict-window drift. Emits
///    `Happening::PluginManifestDrift { admitted: false }` so
///    operators see the refusal reason on the bus.
/// 5. Admits but warns on warn-band: emits
///    `Happening::PluginVersionSkewWarning` and (on drift)
///    `Happening::PluginManifestDrift { admitted: true }`.
///
/// Returns `Ok(())` on admit, `Err(StewardError)` on refusal.
pub(super) async fn check_drift_and_skew(
    manifest: &Manifest,
    runtime: &RuntimeCapabilities,
    bus: &Arc<HappeningBus>,
) -> Result<(), StewardError> {
    let framework_version = framework_version()?;

    let plugin_min = &manifest.prerequisites.evo_min_version;
    let skew = classify_skew(plugin_min, &framework_version);
    let plugin_name = &manifest.plugin.name;

    if matches!(skew, SkewClassification::OutOfWindow) {
        return Err(StewardError::Admission(format!(
            "{}: plugin's prerequisites.evo_min_version \
             {} is too far behind the running framework {} \
             (skew window: current and current-1 strict, \
             current-2 warn-band, current-3 or older refused); \
             rebuild the plugin against a newer evo \
             framework",
            plugin_name, plugin_min, framework_version
        )));
    }

    let drift = detect_drift(manifest, runtime);

    if !drift.is_empty() {
        let admitted_through_drift =
            matches!(skew, SkewClassification::WarnBand);

        let _ = bus
            .emit_durable(Happening::PluginManifestDrift {
                plugin: plugin_name.clone(),
                missing_in_implementation: drift
                    .missing_in_implementation
                    .clone(),
                missing_in_manifest: drift.missing_in_manifest.clone(),
                admitted: admitted_through_drift,
                at: SystemTime::now(),
            })
            .await;

        if !admitted_through_drift {
            return Err(StewardError::Admission(format!(
                "{}: plugin manifest does not match runtime \
                 describe(): missing in implementation = {:?}, \
                 missing in manifest = {:?}; rebuild the plugin \
                 to align manifest declarations with the actual \
                 implementation",
                plugin_name,
                drift.missing_in_implementation,
                drift.missing_in_manifest
            )));
        }
    }

    if matches!(skew, SkewClassification::WarnBand) {
        let skew_minor = skew_minor_versions(plugin_min, &framework_version);
        let _ = bus
            .emit_durable(Happening::PluginVersionSkewWarning {
                plugin: plugin_name.clone(),
                evo_min_version: plugin_min.to_string(),
                skew_minor_versions: skew_minor,
                at: SystemTime::now(),
            })
            .await;
    }

    // Surface the verdict for downstream observers (currently
    // only used for diagnostic purposes; the policy decisions
    // are already captured in the happenings + the
    // refused/admitted return shape).
    let _ = (skew, DriftReport::default());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a manifest by templating into a known-good base. Using
    /// the TOML round-trip mirrors how the steward consumes manifests
    /// in production and avoids hand-constructing the SDK structs
    /// (which carry many fields irrelevant to these checks).
    fn manifest_for(
        admin: bool,
        trust_class: &str,
        instance: &str,
    ) -> Manifest {
        let toml = format!(
            r#"
[plugin]
name = "org.test.plugin"
version = "0.1.0"
contract = 1

[target]
shelf = "test.shelf"
shape = 1

[kind]
instance = "{instance}"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "{trust_class}"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities]
admin = {admin}

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000
"#
        );
        Manifest::from_toml(&toml).expect("manifest should parse")
    }

    #[test]
    fn admin_trust_passes_when_admin_is_false() {
        let m = manifest_for(false, "standard", "singleton");
        assert!(check_admin_trust(&m).is_ok());
    }

    #[test]
    fn admin_trust_passes_when_class_meets_minimum() {
        let m = manifest_for(true, "privileged", "singleton");
        assert!(check_admin_trust(&m).is_ok());
    }

    #[test]
    fn admin_trust_rejects_when_class_below_minimum() {
        let m = manifest_for(true, "standard", "singleton");
        let err = check_admin_trust(&m).expect_err("must reject");
        assert!(matches!(err, StewardError::AdminTrustTooLow { .. }));
    }

    #[test]
    fn framework_version_falls_back_to_cargo_version_when_env_unset() {
        // Tests share the same process env, so guard against a
        // parallel test having set the override and not restored
        // it. `remove_var` is enough; subsequent tests that need
        // the override `set_var` it themselves.
        std::env::remove_var(ENV_FRAMEWORK_VERSION_OVERRIDE);
        let v = framework_version().expect("compiled-in version parses");
        let expected =
            semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        assert_eq!(v, expected);
    }

    #[test]
    fn framework_version_uses_env_override_when_set() {
        std::env::set_var(ENV_FRAMEWORK_VERSION_OVERRIDE, "0.3.0");
        let v = framework_version().expect("override parses");
        assert_eq!(v, semver::Version::new(0, 3, 0));
        std::env::remove_var(ENV_FRAMEWORK_VERSION_OVERRIDE);
    }

    #[test]
    fn framework_version_falls_back_when_env_override_is_garbage() {
        // Acceptance contract: a typo in the override never
        // silently disables version-skew enforcement; the helper
        // logs a warn and falls back to the canonical version so
        // the steward continues to enforce the real bands.
        std::env::set_var(ENV_FRAMEWORK_VERSION_OVERRIDE, "not-a-semver");
        let v = framework_version().expect("fallback succeeds");
        let expected =
            semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        assert_eq!(v, expected);
        std::env::remove_var(ENV_FRAMEWORK_VERSION_OVERRIDE);
    }
}
