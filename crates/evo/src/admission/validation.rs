// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

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
use crate::router::PluginRouter;
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

/// Enforce the inter-plugin dependency declarations from the
/// manifest's `[dependencies]` section.
///
/// Called from every admit entry point AFTER
/// [`check_manifest_prerequisites`] and [`check_admin_trust`].
/// Walks the supplied router state to validate three rules:
///
/// 1. Every entry in `dependencies.required` must currently be
///    admitted on the router. The version requirement (when
///    declared) must match the admitted plugin's version.
///    Missing required dependencies refuse admission with
///    [`StewardError::MissingRequiredDependency`].
/// 2. Every entry in `dependencies.conflicts_with` must NOT
///    currently be admitted. A live conflict refuses with
///    [`StewardError::ConflictingPluginPresent`].
/// 3. Entries in `dependencies.recommended` are advisory; they
///    emit a warn-level tracing record when missing but never
///    refuse admission.
///
/// Pre-existing structural errors (self-dependency,
/// unparseable version, empty entry name) are caught earlier
/// in [`evo_plugin_sdk::manifest::Manifest::validate`] — the
/// runtime check below assumes the structural pass already
/// ran.
pub(super) fn check_dependencies(
    manifest: &Manifest,
    router: &PluginRouter,
) -> Result<(), StewardError> {
    let deps = &manifest.dependencies;

    // Required: every entry must be currently admitted.
    for entry in &deps.required {
        let dep_name = entry.plugin_name();
        let admitted = router.lookup_by_name(dep_name);
        let admitted = match admitted {
            Some(e) => e,
            None => {
                return Err(StewardError::MissingRequiredDependency {
                    plugin_name: manifest.plugin.name.clone(),
                    dependency_name: dep_name.to_string(),
                    detail: "dependency not currently admitted".into(),
                });
            }
        };
        // Version-requirement check (when present).
        if let Some(req_str) = entry.version_str() {
            // Structural-validation guarantees the requirement
            // string parses; defensively match anyway.
            let req = match semver::VersionReq::parse(req_str) {
                Ok(r) => r,
                Err(e) => {
                    return Err(StewardError::MissingRequiredDependency {
                        plugin_name: manifest.plugin.name.clone(),
                        dependency_name: dep_name.to_string(),
                        detail: format!(
                            "dependency version requirement \
                                 {req_str:?} did not parse: {e}"
                        ),
                    });
                }
            };
            // Read the admitted plugin's manifest to compare
            // against. Plugins admitted via the typed
            // in-process entry points may not carry a manifest
            // on the entry; treat that as a version-unknown
            // condition that still satisfies the require-
            // present rule.
            let dep_version = {
                let g =
                    admitted.manifest.lock().expect("manifest mutex poisoned");
                g.as_ref().map(|m| m.plugin.version.clone())
            };
            if let Some(v) = dep_version {
                if !req.matches(&v) {
                    return Err(StewardError::MissingRequiredDependency {
                        plugin_name: manifest.plugin.name.clone(),
                        dependency_name: dep_name.to_string(),
                        detail: format!(
                            "dependency version {v} does not satisfy \
                                 required {req_str}"
                        ),
                    });
                }
            }
        }
    }

    // Conflicts: no entry may currently be admitted.
    for entry in &deps.conflicts_with {
        let dep_name = entry.plugin_name();
        if router.lookup_by_name(dep_name).is_some() {
            return Err(StewardError::ConflictingPluginPresent {
                plugin_name: manifest.plugin.name.clone(),
                conflicting_name: dep_name.to_string(),
            });
        }
    }

    // Recommended: warn-only.
    for entry in &deps.recommended {
        let dep_name = entry.plugin_name();
        if router.lookup_by_name(dep_name).is_none() {
            tracing::warn!(
                plugin = %manifest.plugin.name,
                recommended_dependency = %dep_name,
                "manifest declares recommended dependency that is not \
                 currently admitted; admission proceeds"
            );
        }
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

/// Validate, merge, and admit the plugin's UI stockings.
///
/// Substrate of the UI architecture admission gate. Composes
/// the convergence-default rule with the plugin's explicit
/// `[[ui.stocks]]` from its manifest, validates each candidate
/// stocking against the framework's shelf and widget kind
/// registries (cardinality, accepted widget kinds, accepted
/// sizes, envelope bounds), and records the admitted set
/// against the per-plugin store.
///
/// Merge rule: explicit stockings take precedence. When the
/// convergence default would have surfaced on a shelf the
/// plugin also stocks explicitly, the explicit stocking
/// replaces the default — a plugin that wants the shelf with a
/// non-default widget or non-default size is honoured exactly
/// as declared.
///
/// Cardinality is checked across the full set of admitted
/// stockings on each target shelf (the existing recordings on
/// other plugins, plus this plugin's stockings being admitted).
/// Re-admission of the same plugin (manifest reload) does not
/// double-count the plugin's prior recording: the existing
/// recording is excluded from the count via
/// [`AdmittedStockingsStore::count_on_shelf_excluding`].
///
/// On success, replaces any prior recording for this plugin
/// with the admitted set and returns the admitted set so the
/// caller can surface it to the operator. On failure, the
/// prior recording is left untouched.
///
/// Returns `Ok(Vec::new())` when the plugin admits cleanly but
/// has no UI surface (no convergence default applies and no
/// explicit `[[ui.stocks]]` declared). The empty-vec recording
/// is suppressed (not stored) so the store does not grow with
/// rows for plugins that have no UI.
pub async fn check_ui_stockings(
    plugin_name: &str,
    manifest: &Manifest,
    shelves: &crate::ui_registry::ShelfRegistry,
    widgets: &crate::ui_registry::WidgetKindRegistry,
    admitted: &crate::ui_registry::AdmittedStockingsStore,
) -> Result<Vec<evo_plugin_sdk::ui::UiStocking>, StewardError> {
    use evo_plugin_sdk::ui::{
        validate_cardinality, validate_stocking, AcceptedWidgets,
    };

    // Step 1: derive the convergence default, if any.
    let convergence = crate::ui_convergence::derive_convergence_default(
        &manifest.target.shelf,
        shelves,
        widgets,
    )
    .await
    .map_err(|e| {
        StewardError::Admission(format!(
            "ui admission refused: plugin {plugin_name:?}: convergence \
             default failed: {e}"
        ))
    })?;

    // Step 2: assemble the candidate set. Explicit stockings
    // take precedence; if the plugin explicitly stocks the same
    // shelf the convergence default would have surfaced on, the
    // explicit stocking replaces the default.
    let explicit_shelves: std::collections::HashSet<&str> = manifest
        .ui
        .stocks
        .iter()
        .map(|s| s.ui_shelf.as_str())
        .collect();

    let mut candidates: Vec<evo_plugin_sdk::ui::UiStocking> = Vec::new();
    if let Some(default) = convergence {
        if !explicit_shelves.contains(default.ui_shelf.as_str()) {
            candidates.push(default);
        }
    }
    candidates.extend(manifest.ui.stocks.iter().cloned());

    if candidates.is_empty() {
        // No UI surface for this plugin. Forget any prior
        // recording (a plugin that previously stocked then
        // removed its stockings cleans up).
        admitted.forget(plugin_name).await;
        return Ok(Vec::new());
    }

    // Step 3: validate each candidate against its shelf
    // contract + widget envelope. Refusal classes are
    // structured per the SDK's StockingError taxonomy; the
    // refusal message names the offending plugin, the shelf,
    // and the specific contract rule that failed.
    for stocking in &candidates {
        let shelf = shelves.get(&stocking.ui_shelf).await.ok_or_else(|| {
            StewardError::Admission(format!(
                "ui admission refused: plugin {plugin_name:?}: shelf \
                 {:?} is not registered (no such ui shelf)",
                stocking.ui_shelf
            ))
        })?;
        let widget_kind =
            widgets.get(&stocking.widget).await.ok_or_else(|| {
                StewardError::Admission(format!(
                    "ui admission refused: plugin {plugin_name:?}: widget \
                     kind {:?} (target shelf {:?}) is not registered",
                    stocking.widget, stocking.ui_shelf
                ))
            })?;
        validate_stocking(stocking, &shelf, &widget_kind).map_err(|e| {
            StewardError::Admission(format!(
                "ui admission refused: plugin {plugin_name:?}: {e}"
            ))
        })?;
    }

    // Step 4: cardinality check across the cumulative
    // admitted set. Each candidate stocking against shelf S
    // counts existing stockings on S excluding this plugin's
    // own prior recording, plus the candidates targeting S
    // earlier in the candidates list (so a plugin stocking
    // the same shelf twice on AnyToMany passes; on
    // ExactlyOne the second candidate refuses).
    for (idx, stocking) in candidates.iter().enumerate() {
        let shelf = shelves
            .get(&stocking.ui_shelf)
            .await
            .expect("shelf existence already verified above");
        let earlier_count = candidates[..idx]
            .iter()
            .filter(|s| s.ui_shelf == stocking.ui_shelf)
            .count();
        let existing = admitted
            .count_on_shelf_excluding(&stocking.ui_shelf, plugin_name)
            .await;
        let total_existing = existing + earlier_count;
        validate_cardinality(&shelf, total_existing).map_err(|e| {
            StewardError::Admission(format!(
                "ui admission refused: plugin {plugin_name:?}: {e}"
            ))
        })?;
        // For wildcard-accepts-widgets shelves (vendor
        // catch-all), the per-stocking validate_stocking has
        // already passed; nothing further to do here.
        let _ = AcceptedWidgets::Allowed(Vec::new()); // type sanity
    }

    // Step 5: record the canonical set against the plugin
    // name, replacing any prior recording. Subsequent
    // admissions consult this for cardinality.
    admitted.record(plugin_name, candidates.clone()).await;
    Ok(candidates)
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

    /// Process-wide mutex serialising the three tests that mutate
    /// the framework-version override env var. Without this lock
    /// cargo's parallel test runner interleaves their set/remove
    /// calls and the read-back assertions race against each other.
    /// The defensive `remove_var` at the start of the
    /// unset-asserting test (kept below for documentation) is
    /// necessary but not sufficient: a parallel set-test can still
    /// fire its `set_var` between this test's `remove_var` and
    /// `framework_version()` call. The lock closes that window.
    static ENV_OVERRIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn framework_version_falls_back_to_cargo_version_when_env_unset() {
        let _guard =
            ENV_OVERRIDE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Defensive: a prior test under the same lock may have
        // panicked mid-set. Remove first so the assertion below
        // sees a clean slate even after a poisoned-mutex recovery.
        std::env::remove_var(ENV_FRAMEWORK_VERSION_OVERRIDE);
        let v = framework_version().expect("compiled-in version parses");
        let expected =
            semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        assert_eq!(v, expected);
    }

    #[test]
    fn framework_version_uses_env_override_when_set() {
        let _guard =
            ENV_OVERRIDE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(ENV_FRAMEWORK_VERSION_OVERRIDE, "0.3.0");
        let v = framework_version().expect("override parses");
        assert_eq!(v, semver::Version::new(0, 3, 0));
        std::env::remove_var(ENV_FRAMEWORK_VERSION_OVERRIDE);
    }

    #[test]
    fn framework_version_falls_back_when_env_override_is_garbage() {
        let _guard =
            ENV_OVERRIDE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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

    // ----- check_dependencies tests -----

    fn empty_router() -> PluginRouter {
        PluginRouter::new(crate::state::StewardState::for_tests())
    }

    fn manifest_with_deps(deps_section: &str) -> Manifest {
        let base = format!(
            r#"
[plugin]
name = "org.test.dependent"
version = "0.1.0"
contract = 1

[target]
shelf = "test.shelf"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "standard"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000

{deps_section}
"#
        );
        Manifest::from_toml(&base).expect("manifest should parse")
    }

    #[test]
    fn check_dependencies_empty_section_admits() {
        let m = manifest_with_deps("");
        let r = empty_router();
        check_dependencies(&m, &r).expect("no deps -> ok");
    }

    #[test]
    fn check_dependencies_required_missing_refuses() {
        let m = manifest_with_deps(
            r#"
[dependencies]
required = ["org.example.composition.alsa"]
"#,
        );
        let r = empty_router();
        let err = check_dependencies(&m, &r).unwrap_err();
        assert!(matches!(
            err,
            StewardError::MissingRequiredDependency {
                ref dependency_name,
                ..
            } if dependency_name == "org.example.composition.alsa"
        ));
    }

    #[test]
    fn check_dependencies_recommended_missing_admits_with_warning() {
        // Recommended absence does NOT refuse — only logs at
        // warn level. The test asserts no error.
        let m = manifest_with_deps(
            r#"
[dependencies]
recommended = ["org.example.metadata.local"]
"#,
        );
        let r = empty_router();
        check_dependencies(&m, &r).expect("recommended-missing -> warn");
    }

    #[test]
    fn check_dependencies_conflicts_with_absent_admits() {
        let m = manifest_with_deps(
            r#"
[dependencies]
conflicts_with = ["com.competing.composition"]
"#,
        );
        let r = empty_router();
        check_dependencies(&m, &r).expect("conflict absent -> ok");
    }

    // ----- check_ui_stockings -----

    use crate::ui_registry::{
        AdmittedStockingsStore, ShelfRegistry, WidgetKindRegistry,
    };
    use evo_plugin_sdk::ui::{
        AcceptedWidgets, ShelfCardinality, ShelfContract, ShelfLayout,
        ShelfOrder, UiAspect, UiMode, UiSize, WidgetKindEnvelope,
    };

    /// Build a manifest whose `target.shelf` matches `shelf_id`
    /// and whose `[[ui.stocks]]` is supplied as raw TOML. The
    /// returned manifest passes the SDK's own validation.
    fn manifest_for_ui(shelf_id: &str, ui_blocks: &str) -> Manifest {
        let toml = format!(
            r#"
[plugin]
name = "com.example.ui-stocker"
version = "0.1.0"
contract = 1

[target]
shelf = "{shelf_id}"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "standard"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities]
admin = false

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000
{ui_blocks}
"#
        );
        Manifest::from_toml(&toml).expect("manifest should parse")
    }

    fn ui_shelf(
        id: &str,
        cardinality: ShelfCardinality,
        default_widget: Option<&str>,
        widgets: AcceptedWidgets,
        sizes: Vec<UiSize>,
    ) -> ShelfContract {
        ShelfContract {
            id: id.into(),
            label: Some(id.into()),
            cardinality,
            accepts_widgets: widgets,
            accepts_sizes: sizes,
            layout: ShelfLayout::Grid,
            order_by: ShelfOrder::ManifestDeclaration,
            default_widget: default_widget.map(String::from),
            schema_version: 1,
            min_compatible_version: None,
        }
    }

    fn ui_widget(
        id: &str,
        min: UiSize,
        ideal: UiSize,
        max: UiSize,
    ) -> WidgetKindEnvelope {
        WidgetKindEnvelope {
            id: id.into(),
            min_size: min,
            ideal_size: ideal,
            max_size: max,
            aspect_ratio: UiAspect::Wide,
            responsive: std::collections::BTreeMap::new(),
            mode: UiMode::Inline,
            schema_version: 1,
        }
    }

    #[tokio::test]
    async fn ui_admits_empty_set_when_no_shelves_registered() {
        // Plugin has no [[ui.stocks]] and the framework has no
        // shelves registered yet — the admit path must succeed
        // with an empty result and not record anything.
        let m = manifest_for_ui("metadata.providers", "");
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        let result = check_ui_stockings(
            "com.example.ui-stocker",
            &m,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .expect("empty UI surface admits cleanly");
        assert!(result.is_empty());
        assert!(admitted.is_empty().await);
    }

    #[tokio::test]
    async fn ui_admits_convergence_default_when_shelf_carries_one() {
        let m = manifest_for_ui("metadata.providers", "");
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        shelves
            .register(ui_shelf(
                "metadata.providers",
                ShelfCardinality::AnyToMany,
                Some("audio.metadata.entry"),
                AcceptedWidgets::Allowed(vec!["audio.metadata.entry".into()]),
                vec![UiSize::Third, UiSize::Half],
            ))
            .await
            .unwrap();
        widgets
            .register(ui_widget(
                "audio.metadata.entry",
                UiSize::Third,
                UiSize::Half,
                UiSize::Full,
            ))
            .await
            .unwrap();
        let result = check_ui_stockings(
            "com.example.ui-stocker",
            &m,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .expect("convergence default admits");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].ui_shelf, "metadata.providers");
        assert_eq!(result[0].widget, "audio.metadata.entry");
        assert_eq!(result[0].size, UiSize::Half);
        // Recording was made.
        let recorded = admitted.get("com.example.ui-stocker").await.unwrap();
        assert_eq!(recorded.len(), 1);
    }

    #[tokio::test]
    async fn ui_explicit_stocking_overrides_convergence_default_on_same_shelf()
    {
        let ui_block = r#"
[[ui.stocks]]
ui_shelf       = "metadata.providers"
widget         = "audio.metadata.rich"
size           = "full"
schema_version = 1
"#;
        let m = manifest_for_ui("metadata.providers", ui_block);
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        shelves
            .register(ui_shelf(
                "metadata.providers",
                ShelfCardinality::AnyToMany,
                Some("audio.metadata.entry"),
                AcceptedWidgets::Allowed(vec![
                    "audio.metadata.entry".into(),
                    "audio.metadata.rich".into(),
                ]),
                vec![UiSize::Third, UiSize::Half, UiSize::Full],
            ))
            .await
            .unwrap();
        widgets
            .register(ui_widget(
                "audio.metadata.entry",
                UiSize::Third,
                UiSize::Half,
                UiSize::Full,
            ))
            .await
            .unwrap();
        widgets
            .register(ui_widget(
                "audio.metadata.rich",
                UiSize::Half,
                UiSize::Full,
                UiSize::Full,
            ))
            .await
            .unwrap();
        let result = check_ui_stockings(
            "com.example.ui-stocker",
            &m,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .expect("explicit override admits");
        // Only one stocking — the explicit one. Convergence
        // default is suppressed because the explicit stocking
        // targets the same shelf.
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].widget, "audio.metadata.rich");
        assert_eq!(result[0].size, UiSize::Full);
    }

    #[tokio::test]
    async fn ui_refuses_unknown_shelf_in_explicit_stocking() {
        let ui_block = r#"
[[ui.stocks]]
ui_shelf       = "ghost.shelf"
widget         = "any.widget"
size           = "third"
schema_version = 1
"#;
        let m = manifest_for_ui("metadata.providers", ui_block);
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        let err = check_ui_stockings(
            "com.example.ui-stocker",
            &m,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ghost.shelf"));
        assert!(msg.contains("not registered"));
    }

    #[tokio::test]
    async fn ui_refuses_cardinality_overflow_on_exactly_one_shelf() {
        // First plugin admits onto an ExactlyOne shelf;
        // second plugin's stocking on the same shelf refuses.
        let ui_block_a = r#"
[[ui.stocks]]
ui_shelf       = "now_playing.player"
widget         = "audio.player.transport"
size           = "full"
schema_version = 1
"#;
        let m_a = Manifest::from_toml(&format!(
            r#"
[plugin]
name = "com.example.player-a"
version = "0.1.0"
contract = 1

[target]
shelf = "audio.player"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "standard"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities]
admin = false

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000
{ui_block_a}
"#
        ))
        .expect("manifest a parses");
        let m_b = manifest_for_ui("audio.player", ui_block_a);

        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        shelves
            .register(ui_shelf(
                "now_playing.player",
                ShelfCardinality::ExactlyOne,
                None, // no convergence default — explicit stocking only
                AcceptedWidgets::Allowed(vec!["audio.player.transport".into()]),
                vec![UiSize::Full],
            ))
            .await
            .unwrap();
        widgets
            .register(ui_widget(
                "audio.player.transport",
                UiSize::Full,
                UiSize::Full,
                UiSize::Full,
            ))
            .await
            .unwrap();

        // Plugin A admits cleanly.
        check_ui_stockings(
            "com.example.player-a",
            &m_a,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .expect("plugin a admits onto ExactlyOne shelf");
        assert_eq!(admitted.count_on_shelf("now_playing.player").await, 1);

        // Plugin B refuses — cardinality exceeded.
        let err = check_ui_stockings(
            "com.example.player-b",
            &m_b,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("cardinality"));
        // Plugin B's recording was NOT made.
        assert!(admitted.get("com.example.player-b").await.is_none());
    }

    #[tokio::test]
    async fn ui_re_admission_excludes_plugins_own_prior_recording() {
        // Plugin admits, then admits again with a different
        // stocking — second admission must NOT count the
        // plugin's own prior stocking against itself.
        let ui_block = r#"
[[ui.stocks]]
ui_shelf       = "now_playing.player"
widget         = "audio.player.transport"
size           = "full"
schema_version = 1
"#;
        let m = manifest_for_ui("audio.player", ui_block);
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        shelves
            .register(ui_shelf(
                "now_playing.player",
                ShelfCardinality::ExactlyOne,
                None,
                AcceptedWidgets::Allowed(vec!["audio.player.transport".into()]),
                vec![UiSize::Full],
            ))
            .await
            .unwrap();
        widgets
            .register(ui_widget(
                "audio.player.transport",
                UiSize::Full,
                UiSize::Full,
                UiSize::Full,
            ))
            .await
            .unwrap();
        // First admission.
        check_ui_stockings(
            "com.example.ui-stocker",
            &m,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .expect("first admission ok");
        // Second admission — same plugin, same stocking.
        // Cardinality check excludes the plugin's prior
        // recording; admission succeeds.
        check_ui_stockings(
            "com.example.ui-stocker",
            &m,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .expect("re-admission must not count plugin's own prior recording");
    }

    #[tokio::test]
    async fn ui_no_surface_forgets_prior_recording() {
        // Plugin previously admitted with a stocking; a new
        // manifest reload removes the [[ui.stocks]]. The
        // forget-on-empty rule cleans up the prior recording.
        let ui_block = r#"
[[ui.stocks]]
ui_shelf       = "library.sources"
widget         = "audio.browse.tree.entry"
size           = "third"
schema_version = 1
"#;
        let m_with_ui = manifest_for_ui("library.sources", ui_block);
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        shelves
            .register(ui_shelf(
                "library.sources",
                ShelfCardinality::AnyToMany,
                None,
                AcceptedWidgets::Allowed(
                    vec!["audio.browse.tree.entry".into()],
                ),
                vec![UiSize::Third, UiSize::Half],
            ))
            .await
            .unwrap();
        widgets
            .register(ui_widget(
                "audio.browse.tree.entry",
                UiSize::Third,
                UiSize::Half,
                UiSize::Full,
            ))
            .await
            .unwrap();
        check_ui_stockings(
            "com.example.ui-stocker",
            &m_with_ui,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .unwrap();
        assert!(admitted.get("com.example.ui-stocker").await.is_some());

        // Reload with no UI surface.
        let m_no_ui = manifest_for_ui("non.ui.shelf", "");
        let result = check_ui_stockings(
            "com.example.ui-stocker",
            &m_no_ui,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .expect("no-ui reload admits");
        assert!(result.is_empty());
        assert!(admitted.get("com.example.ui-stocker").await.is_none());
    }

    /// Exercise the cumulative cardinality count when the
    /// same plugin's manifest stocks the same shelf twice on
    /// an AnyToMany shelf — must succeed.
    #[tokio::test]
    async fn ui_admits_two_stockings_on_any_to_many_in_one_manifest() {
        let ui_block = r#"
[[ui.stocks]]
ui_shelf       = "library.sources"
widget         = "audio.browse.tree.entry"
size           = "third"
schema_version = 1

[[ui.stocks]]
ui_shelf       = "library.sources"
widget         = "audio.browse.tree.entry"
size           = "half"
schema_version = 1
"#;
        let m = manifest_for_ui("non.ui.shelf", ui_block);
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        shelves
            .register(ui_shelf(
                "library.sources",
                ShelfCardinality::AnyToMany,
                None,
                AcceptedWidgets::Allowed(
                    vec!["audio.browse.tree.entry".into()],
                ),
                vec![UiSize::Third, UiSize::Half],
            ))
            .await
            .unwrap();
        widgets
            .register(ui_widget(
                "audio.browse.tree.entry",
                UiSize::Third,
                UiSize::Half,
                UiSize::Full,
            ))
            .await
            .unwrap();
        let result = check_ui_stockings(
            "com.example.ui-stocker",
            &m,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .expect("two stockings on AnyToMany admit");
        assert_eq!(result.len(), 2);
    }

    /// Same shape but the shelf is ExactlyOne — second
    /// stocking in the same manifest must refuse.
    #[tokio::test]
    async fn ui_refuses_two_stockings_on_exactly_one_in_one_manifest() {
        let ui_block = r#"
[[ui.stocks]]
ui_shelf       = "now_playing.player"
widget         = "audio.player.transport"
size           = "full"
schema_version = 1

[[ui.stocks]]
ui_shelf       = "now_playing.player"
widget         = "audio.player.transport"
size           = "full"
schema_version = 1
"#;
        let m = manifest_for_ui("non.ui.shelf", ui_block);
        let shelves = ShelfRegistry::new();
        let widgets = WidgetKindRegistry::new();
        let admitted = AdmittedStockingsStore::new();
        shelves
            .register(ui_shelf(
                "now_playing.player",
                ShelfCardinality::ExactlyOne,
                None,
                AcceptedWidgets::Allowed(vec!["audio.player.transport".into()]),
                vec![UiSize::Full],
            ))
            .await
            .unwrap();
        widgets
            .register(ui_widget(
                "audio.player.transport",
                UiSize::Full,
                UiSize::Full,
                UiSize::Full,
            ))
            .await
            .unwrap();
        let err = check_ui_stockings(
            "com.example.ui-stocker",
            &m,
            &shelves,
            &widgets,
            &admitted,
        )
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("cardinality"));
    }
}
