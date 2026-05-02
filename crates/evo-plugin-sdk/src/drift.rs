//! Manifest-drift detection.
//!
//! Compares a plugin's manifest declarations against its runtime
//! [`describe`] response. A mismatch — drift — indicates a plugin
//! authoring bug: the manifest claims a verb the implementation
//! does not provide, or the implementation provides a verb the
//! manifest does not advertise. Either case lets the wrong verb
//! pass the framework's dispatch gate (or the wrong verb get
//! refused unnecessarily); both are bugs.
//!
//! This module is the trait-free, dependency-free comparator used
//! at three tiers:
//!
//! - **Admission-time** (in the framework's admit path): refuse on
//!   drift in the strict-window of the version-skew policy; warn
//!   in the warn-band.
//! - **CI-time** (via `evo-plugin-test`): plugin authors call
//!   `assert_manifest_matches_describe(plugin)` from unit tests so
//!   drift surfaces before the bundle is signed or published.
//! - **Sign-time** (via `evo-plugin-tool`): the verify command
//!   refuses to admit a drifted bundle.
//!
//! [`describe`]: crate::contract::Plugin::describe

use std::collections::BTreeSet;

use crate::contract::RuntimeCapabilities;
use crate::manifest::Manifest;

use serde::{Deserialize, Serialize};

/// Drift report comparing a plugin's manifest declaration to its
/// runtime `describe()` response. Empty when no drift is observed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftReport {
    /// Verbs declared in the manifest but absent from the plugin's
    /// runtime `describe()`. The plugin advertises support it does
    /// not actually provide.
    pub missing_in_implementation: Vec<String>,
    /// Verbs reported by the plugin's runtime `describe()` but
    /// absent from the manifest. The plugin implements support it
    /// did not advertise.
    pub missing_in_manifest: Vec<String>,
}

impl DriftReport {
    /// `true` when no drift is observed in either direction.
    pub fn is_empty(&self) -> bool {
        self.missing_in_implementation.is_empty()
            && self.missing_in_manifest.is_empty()
    }
}

/// Compare the plugin's manifest declarations against its
/// runtime `describe()` response and return a [`DriftReport`].
///
/// The check covers:
///
/// - Respondents: `manifest.capabilities.respondent.request_types`
///   vs `runtime.request_types`.
/// - Wardens: `manifest.capabilities.warden.course_correct_verbs`
///   (when set) vs `runtime.course_correct_verbs`. A `None` in the
///   manifest is treated as "no declaration" — drift detection
///   skips the comparison; legacy plugins authored before the
///   field existed pass without comparison. Plugins targeting
///   recent `evo_min_version` are expected to declare the field;
///   the version-skew policy decides admission consequences.
pub fn detect_drift(
    manifest: &Manifest,
    runtime: &RuntimeCapabilities,
) -> DriftReport {
    let mut report = DriftReport::default();

    if let Some(r) = manifest.capabilities.respondent.as_ref() {
        let manifest_set: BTreeSet<&str> =
            r.request_types.iter().map(|s| s.as_str()).collect();
        let runtime_set: BTreeSet<&str> =
            runtime.request_types.iter().map(|s| s.as_str()).collect();

        for v in manifest_set.difference(&runtime_set) {
            report.missing_in_implementation.push((*v).to_string());
        }
        for v in runtime_set.difference(&manifest_set) {
            report.missing_in_manifest.push((*v).to_string());
        }
    }

    if let Some(verbs) = manifest
        .capabilities
        .warden
        .as_ref()
        .and_then(|w| w.course_correct_verbs.as_ref())
    {
        let manifest_set: BTreeSet<&str> =
            verbs.iter().map(|s| s.as_str()).collect();
        let runtime_set: BTreeSet<&str> = runtime
            .course_correct_verbs
            .iter()
            .map(|s| s.as_str())
            .collect();

        for v in manifest_set.difference(&runtime_set) {
            report.missing_in_implementation.push((*v).to_string());
        }
        for v in runtime_set.difference(&manifest_set) {
            report.missing_in_manifest.push((*v).to_string());
        }
    }

    report.missing_in_implementation.sort();
    report.missing_in_manifest.sort();
    report
}
