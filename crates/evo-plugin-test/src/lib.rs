//! CI-time helpers for plugin authors.
//!
//! The framework's admission engine refuses to admit a plugin
//! whose manifest declarations do not match its runtime
//! `describe()` response (drift). Plugin authors discover drift
//! either when their plugin reaches a deployment (admission
//! refuses, unhappy operator) or, with this crate, in their unit
//! tests (assertion fires, plugin author fixes before publish).
//!
//! This crate is the CI-time tier of the three-tier drift safety
//! net. Sign-time and admission-time are the other two.
//!
//! ## Usage
//!
//! Add the crate to a plugin's `[dev-dependencies]`:
//!
//! ```toml
//! [dev-dependencies]
//! evo-plugin-test = "0.1"
//! ```
//!
//! In a unit test, construct the plugin and the manifest the same
//! way the framework would, then call the assertion:
//!
//! ```ignore
//! #[tokio::test]
//! async fn manifest_matches_implementation() {
//!     let plugin = MyPlugin::new();
//!     let manifest = include_str!("../manifest.toml")
//!         .parse::<evo_plugin_sdk::Manifest>()
//!         .unwrap();
//!     evo_plugin_test::assert_manifest_matches_describe(
//!         &manifest,
//!         &plugin,
//!     )
//!     .await;
//! }
//! ```
//!
//! The assertion fires (test fails) on drift in either direction
//! with a structured panic message naming the missing verbs.
//!
//! For plugin authors who want to inspect the drift report rather
//! than panic, [`compute_drift`] returns the same
//! [`DriftReport`](evo_plugin_sdk::drift::DriftReport) the
//! framework's admission engine consumes.

#![warn(missing_docs)]

use evo_plugin_sdk::contract::Plugin;
use evo_plugin_sdk::drift::{detect_drift, DriftReport};
use evo_plugin_sdk::manifest::Manifest;

/// Compute a [`DriftReport`] for `plugin` against `manifest`.
///
/// Calls `plugin.describe()` and compares the returned
/// `RuntimeCapabilities` against the manifest's declared
/// `capabilities.respondent.request_types` and (when set)
/// `capabilities.warden.course_correct_verbs`. Returns the
/// report directly without panicking; callers wanting an
/// assertion shape should use
/// [`assert_manifest_matches_describe`].
pub async fn compute_drift<P: Plugin + ?Sized>(
    manifest: &Manifest,
    plugin: &P,
) -> DriftReport {
    let description = plugin.describe().await;
    detect_drift(manifest, &description.runtime_capabilities)
}

/// Assert that `plugin`'s `describe()` response matches the
/// manifest's declared verb sets. Panics on drift in either
/// direction with a structured message naming the missing verbs.
///
/// Designed for use in plugin authors' unit tests
/// (`#[tokio::test]` recommended). Catches drift before sign /
/// publish / admit.
pub async fn assert_manifest_matches_describe<P: Plugin + ?Sized>(
    manifest: &Manifest,
    plugin: &P,
) {
    let report = compute_drift(manifest, plugin).await;
    if report.is_empty() {
        return;
    }
    panic!(
        "manifest does not match plugin describe(): \
         missing in implementation = {:?}, \
         missing in manifest = {:?}; \
         update the manifest to match what the plugin actually \
         provides, or update the plugin to provide what the \
         manifest declares — the framework's admission engine \
         refuses drifted plugins in the strict-window of the \
         version-skew policy",
        report.missing_in_implementation, report.missing_in_manifest
    );
}
