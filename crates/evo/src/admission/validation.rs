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
    let evo_version = match semver::Version::parse(env!("CARGO_PKG_VERSION")) {
        Ok(v) => v,
        Err(e) => {
            return Err(StewardError::Admission(format!(
                "evo's own CARGO_PKG_VERSION is not valid semver: {e}"
            )));
        }
    };
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
    let framework_version =
        match semver::Version::parse(env!("CARGO_PKG_VERSION")) {
            Ok(v) => v,
            Err(e) => {
                return Err(StewardError::Admission(format!(
                    "evo's own CARGO_PKG_VERSION is not valid \
                     semver: {e}"
                )));
            }
        };

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
}
