//! Version-skew classification per the K8s-style `+/- 1 minor`
//! policy.
//!
//! Each admitted plugin declares a
//! `prerequisites.evo_min_version` — the minimum framework
//! version the plugin is built against. The framework runs at one
//! version; the difference between the framework's version and
//! the plugin's declared minimum is the skew. The classification
//! function in this module maps that skew onto a four-state
//! enforcement decision:
//!
//! - **`Strict`** — plugin's `evo_min_version` is the framework's
//!   current minor or `current - 1`. Drift detection refuses on
//!   mismatch; new mandatory fields are enforced; full validation
//!   applies.
//! - **`WarnBand`** — plugin's `evo_min_version` is `current - 2`.
//!   The plugin is admitted; the framework emits a
//!   `PluginVersionSkewWarning` happening so operators can plan a
//!   refresh; drift detection downgrades to a warning rather than
//!   refusal; new fields are treated as optional.
//! - **`OutOfWindow`** — plugin's `evo_min_version` is `current
//!   - 3` or older. Admission is refused with a structured
//!   `permission_denied / version_skew_too_wide` error.
//! - **`TooNew`** — plugin's `evo_min_version` is greater than the
//!   framework's version. Admission refuses with the existing
//!   `EvoVersionTooLow` check; this classifier surfaces the case
//!   for completeness.
//!
//! The classifier ignores patch and pre-release components — only
//! major + minor matter for the skew window. Pre-1.0 versions
//! treat the major component as fixed at 0 and use the minor
//! component as the skew dimension.
//!
//! ## Time-decoupled refinement (future work)
//!
//! A future refinement adds a calendar-based component to the
//! window: plugins whose required minimum framework version's
//! release date is more than 18 months old are refused
//! regardless of version-count, and 12-18 months are warn-band.
//! For the project's current age the time component is dormant —
//! every prior framework version was released within a few weeks
//! and the calendar refinement only kicks in once the project
//! ages. This module's API today returns the version-count
//! classification alone; future integration extends
//! [`classify_skew`]'s signature with a release-date map without
//! changing existing callers.

use semver::Version;
use serde::{Deserialize, Serialize};

/// Outcome of classifying one plugin's `evo_min_version` against
/// the running framework's version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkewClassification {
    /// Plugin requires a newer framework than the one running.
    /// Refused at admit per the existing `EvoVersionTooLow`
    /// check; this classifier surfaces the case for completeness.
    TooNew,
    /// Plugin is at the framework's current minor or one minor
    /// behind. Strict enforcement applies; drift refuses on
    /// mismatch; new mandatory fields are enforced.
    Strict,
    /// Plugin is two minor versions behind the framework.
    /// Admission proceeds with a warning happening; drift is
    /// downgraded to a warning; new fields are treated as
    /// optional with backward-compat defaults.
    WarnBand,
    /// Plugin is three or more minor versions behind. Admission
    /// is refused with `permission_denied / version_skew_too_wide`.
    OutOfWindow,
}

impl SkewClassification {
    /// Wire-form name. Stable across releases.
    pub fn as_str(&self) -> &'static str {
        match self {
            SkewClassification::TooNew => "too_new",
            SkewClassification::Strict => "strict",
            SkewClassification::WarnBand => "warn_band",
            SkewClassification::OutOfWindow => "out_of_window",
        }
    }

    /// `true` when the classification permits admission to
    /// proceed (`Strict` or `WarnBand`).
    pub fn admits(&self) -> bool {
        matches!(
            self,
            SkewClassification::Strict | SkewClassification::WarnBand
        )
    }
}

/// Classify a plugin's `evo_min_version` against the running
/// framework's version.
///
/// The skew is the difference in minor-version count between the
/// framework and the plugin's declared minimum (with major
/// version held fixed; cross-major skew is not addressed by this
/// classifier and is expected to be a refusal at the manifest
/// schema layer).
pub fn classify_skew(
    plugin_min: &Version,
    framework: &Version,
) -> SkewClassification {
    if plugin_min.major != framework.major {
        // Cross-major skew is out-of-band for this classifier.
        // Plugin major > framework major: plugin requires a
        // newer framework. Plugin major < framework major: too
        // far behind to be expressible in the +/- 1 minor
        // window.
        return if plugin_min.major > framework.major {
            SkewClassification::TooNew
        } else {
            SkewClassification::OutOfWindow
        };
    }

    match framework.minor.cmp(&plugin_min.minor) {
        std::cmp::Ordering::Less => SkewClassification::TooNew,
        std::cmp::Ordering::Equal => SkewClassification::Strict,
        std::cmp::Ordering::Greater => {
            let delta = framework.minor - plugin_min.minor;
            match delta {
                1 => SkewClassification::Strict,
                2 => SkewClassification::WarnBand,
                _ => SkewClassification::OutOfWindow,
            }
        }
    }
}

/// How many minor versions the plugin is behind the framework
/// (zero or positive). Returns 0 when the plugin requires a newer
/// framework (caller distinguishes via `classify_skew`).
pub fn skew_minor_versions(plugin_min: &Version, framework: &Version) -> u32 {
    if plugin_min.major != framework.major || framework.minor < plugin_min.minor
    {
        return 0;
    }
    (framework.minor - plugin_min.minor) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u64, minor: u64) -> Version {
        Version::new(major, minor, 0)
    }

    #[test]
    fn same_minor_is_strict() {
        assert_eq!(
            classify_skew(&v(0, 1), &v(0, 1)),
            SkewClassification::Strict
        );
    }

    #[test]
    fn one_behind_is_strict() {
        // v0.1.12 framework; plugin built for v0.1.11.
        assert_eq!(
            classify_skew(&v(0, 11), &v(0, 12)),
            SkewClassification::Strict
        );
    }

    #[test]
    fn two_behind_is_warn_band() {
        // v0.1.12 framework; plugin built for v0.1.10.
        assert_eq!(
            classify_skew(&v(0, 10), &v(0, 12)),
            SkewClassification::WarnBand
        );
    }

    #[test]
    fn three_behind_is_out_of_window() {
        // v0.1.12 framework; plugin built for v0.1.9.
        assert_eq!(
            classify_skew(&v(0, 9), &v(0, 12)),
            SkewClassification::OutOfWindow
        );
    }

    #[test]
    fn six_behind_is_out_of_window() {
        assert_eq!(
            classify_skew(&v(0, 6), &v(0, 12)),
            SkewClassification::OutOfWindow
        );
    }

    #[test]
    fn plugin_newer_than_framework_is_too_new() {
        assert_eq!(
            classify_skew(&v(0, 13), &v(0, 12)),
            SkewClassification::TooNew
        );
    }

    #[test]
    fn cross_major_higher_is_too_new() {
        assert_eq!(
            classify_skew(&v(1, 0), &v(0, 99)),
            SkewClassification::TooNew
        );
    }

    #[test]
    fn cross_major_lower_is_out_of_window() {
        assert_eq!(
            classify_skew(&v(0, 99), &v(1, 0)),
            SkewClassification::OutOfWindow
        );
    }

    #[test]
    fn admits_only_strict_and_warn_band() {
        assert!(SkewClassification::Strict.admits());
        assert!(SkewClassification::WarnBand.admits());
        assert!(!SkewClassification::OutOfWindow.admits());
        assert!(!SkewClassification::TooNew.admits());
    }

    #[test]
    fn skew_minor_versions_basics() {
        assert_eq!(skew_minor_versions(&v(0, 12), &v(0, 12)), 0);
        assert_eq!(skew_minor_versions(&v(0, 11), &v(0, 12)), 1);
        assert_eq!(skew_minor_versions(&v(0, 10), &v(0, 12)), 2);
        assert_eq!(skew_minor_versions(&v(0, 13), &v(0, 12)), 0);
    }

    #[test]
    fn as_str_round_trips() {
        assert_eq!(SkewClassification::Strict.as_str(), "strict");
        assert_eq!(SkewClassification::WarnBand.as_str(), "warn_band");
        assert_eq!(SkewClassification::OutOfWindow.as_str(), "out_of_window");
        assert_eq!(SkewClassification::TooNew.as_str(), "too_new");
    }
}
