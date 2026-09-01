// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Three-channel update model SDK contract.
//!
//! evo devices receive updates from three independent
//! sources — plugins (via registries), evo-core itself
//! (via the evo-core-artefacts release channel), and the
//! underlying OS (via the distribution's package manager).
//! Each source has its own check + apply mechanism, restart
//! implications, and audit posture, but operators see one
//! unified inventory + one operator-facing dashboard + one
//! audit trail.
//!
//! The framework runs the orchestration shell; each source
//! plugin owns its specific apply mechanism by implementing
//! the [`UpdateSource`] trait. The framework's
//! `UpdateRegistry` manages registered sources, aggregates
//! their inventories, schedules background checks, and
//! drives the operator surface.
//!
//! ## Trait shape
//!
//! Implementors return:
//!
//! - A stable [`SourceId`] — `"plugins"` / `"core"` / `"os"`
//!   are the framework-known canonical ids; vendors may
//!   register additional sources (e.g. `"firmware"` for a
//!   Pi HAT firmware updater) by minting their own.
//! - A [`SourceCapabilities`] declaration describing what
//!   the source can do (background-check, atomic apply,
//!   rollback, size estimate) so the framework can
//!   present accurate UI affordances.
//! - An async `check_for_updates` method returning every
//!   update currently available from this source.
//! - An async `apply_update` method that actually applies
//!   one update, returning structured progress + outcome.
//!
//! Sources are admitted as plugins; the framework's
//! admission engine recognises plugins implementing this
//! trait and registers them in the update registry.

use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Stable identifier for one update source. Framework-
/// canonical ids: `"plugins"` (registry-driven plugin
/// updates), `"core"` (evo-core binary updates from the
/// release channel), `"os"` (distribution package-manager
/// updates). Vendors may register additional sources by
/// minting their own ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub String);

impl SourceId {
    /// Construct a source id from a static string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the canonical id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable identifier for one specific update offered by a
/// source. Ids are source-defined; the framework treats
/// them as opaque keys. Convention: `<component>@<version>`
/// (e.g. `com.tidal@1.3.0`, `evo@0.1.13.1`,
/// `linux-image@6.1.0-27`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UpdateId(pub String);

impl UpdateId {
    /// Construct an update id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the canonical id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Severity classification for one update. Drives operator
/// presentation + auto-apply policy thresholds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSeverity {
    /// Routine maintenance update — feature additions, minor
    /// fixes, refresh of bundled content.
    Routine,
    /// Recommended update — backwards-compatible
    /// improvements the operator would benefit from but is
    /// not urgent.
    Recommended,
    /// Security update — addresses a known vulnerability.
    /// Operator surfaces highlight; auto-apply policy
    /// typically allows unattended application.
    Security,
    /// Critical update — actively-exploited vulnerability or
    /// data-loss bug. Operator surfaces alarm; auto-apply
    /// policy typically allows unattended application.
    Critical,
}

/// Restart implications of one update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartLevel {
    /// No restart needed. The update applies in-place
    /// without disturbing any running component.
    None,
    /// One plugin restarts after the update applies.
    Plugin,
    /// The steward itself performs a graceful restart
    /// preserving subjects / queues / custody / happenings
    /// cursor.
    Steward,
    /// Full device reboot required (kernel update, firmware
    /// flash, etc.).
    Reboot,
}

/// Capabilities one source declares to the framework.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCapabilities {
    /// `true` when the source can run unattended periodic
    /// checks (most sources). `false` when checks must be
    /// operator-initiated (e.g. expensive on-demand
    /// firmware queries).
    pub background_check: bool,
    /// `true` when individual `apply_update` calls are
    /// atomic — either they fully apply or leave the system
    /// in the prior state. Plugin updates are typically
    /// atomic; package-manager OS updates typically are
    /// not.
    pub atomic_apply: bool,
    /// Worst-case restart level any update from this source
    /// might require. Framework uses this for UI hints
    /// before knowing the per-update specifics.
    pub requires_restart: RestartLevel,
    /// `true` when the source supports rollback to the
    /// prior version after a failed apply.
    pub rollback_supported: bool,
    /// `true` when the source can estimate the on-disk /
    /// download size of an update before applying. UI
    /// surfaces this to operators so they can budget.
    pub size_estimate: bool,
}

/// One available update reported by a source's `check_for_updates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateAvailable {
    /// Stable update id within the source's namespace.
    pub id: UpdateId,
    /// Component name (e.g. `com.tidal`, `evo`,
    /// `linux-image`). Vendor-defined; the framework
    /// surfaces it in operator UI.
    pub component: String,
    /// Currently-installed version string. Format is
    /// component-defined (semver, distribution version
    /// string, etc.).
    pub current_version: String,
    /// Available version string.
    pub available_version: String,
    /// Optional URL pointing at a changelog / release notes
    /// for the update. UI links to it; the framework does
    /// not fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changelog_url: Option<String>,
    /// Severity classification.
    pub severity: UpdateSeverity,
    /// Estimated size in bytes when the source supports
    /// size estimates (per [`SourceCapabilities::size_estimate`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Restart level required to apply this specific update.
    /// May be lower than the source's worst-case.
    pub requires_restart: RestartLevel,
    /// When the upstream published the update.
    pub published_at: SystemTime,
}

/// Options the operator passes to `apply_update`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApplyOptions {
    /// When `true`, the source should perform any
    /// reversible apply steps but stop before committing
    /// — useful for operator dry-run before approving.
    /// Sources without a meaningful dry-run path may treat
    /// this as a request for a no-op success and document
    /// accordingly.
    #[serde(default)]
    pub dry_run: bool,
    /// Operator-supplied principal — recorded in the
    /// action ledger as the apply approver. Empty when the
    /// apply was triggered by the auto-apply policy
    /// (framework records `auto_apply` in that case).
    #[serde(default)]
    pub approved_by: Option<String>,
}

/// Outcome of a successful `apply_update`. Failures are
/// reported via [`UpdateError`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateOutcome {
    /// Echo of the update id that was applied.
    pub id: UpdateId,
    /// Component name.
    pub component: String,
    /// Version that is now active after the apply.
    pub applied_version: String,
    /// Whether the apply triggered the declared restart
    /// level (so the operator knows to expect a follow-on
    /// disruption).
    pub restart_initiated: RestartLevel,
    /// Whether this was a dry-run. Dry-run outcomes never
    /// change installed versions; the field surfaces the
    /// distinction in the audit trail.
    #[serde(default)]
    pub dry_run: bool,
}

/// Errors raised by [`UpdateSource`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// Source could not reach its upstream (registry,
    /// release channel, package mirror, etc.).
    #[error("update source unreachable: {0}")]
    SourceUnreachable(String),
    /// The update id is unknown or no longer available.
    #[error("update id unknown or no longer available: {0}")]
    UnknownUpdate(String),
    /// Signature / integrity verification failed.
    #[error("update signature verification failed: {0}")]
    SignatureInvalid(String),
    /// The apply path failed mid-flight; the system may
    /// have been left in a partial state if the source's
    /// capabilities did not declare atomic apply.
    #[error("update apply failed: {0}")]
    ApplyFailed(String),
    /// The source refused the request because the operator
    /// lacks an authorisation prerequisite (e.g. step-up
    /// auth) the source enforces independently.
    #[error("update apply not authorised: {0}")]
    NotAuthorised(String),
    /// Internal source error — the source itself is broken,
    /// not the upstream. Distinct from `SourceUnreachable`
    /// (which is recoverable by retry / connectivity fix).
    #[error("update source internal error: {0}")]
    Internal(String),
}

/// SDK trait every update source plugin implements. The
/// framework's update registry calls these methods on a
/// schedule (background check) or on demand (operator-
/// initiated check / apply).
///
/// Implementations are expected to be cheap to construct
/// (heavy state initialisation should land in the source
/// plugin's normal admission flow). The async methods
/// return boxed futures rather than `async fn` so the
/// trait stays object-safe — the framework registry stores
/// every source as `Arc<dyn UpdateSource>` and calls
/// through a uniform dispatch path. This matches the
/// boxed-future convention every other SDK callback trait
/// uses.
pub trait UpdateSource: Send + Sync {
    /// Stable source id. Must be unique per registered
    /// source; the framework refuses double-registration.
    fn source_id(&self) -> SourceId;

    /// Operator-facing display name.
    fn display_name(&self) -> String;

    /// Capability declaration — see [`SourceCapabilities`].
    fn capabilities(&self) -> SourceCapabilities;

    /// Check the source's upstream for available updates.
    /// Returns every update currently available; the
    /// framework deduplicates against the prior inventory
    /// and emits the appropriate happenings.
    fn check_for_updates<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<UpdateAvailable>, UpdateError>>
                + Send
                + 'a,
        >,
    >;

    /// Apply one specific update. Sources may use
    /// `options.dry_run` to perform a reversible test;
    /// `options.approved_by` records the apply approver in
    /// the action ledger.
    fn apply_update<'a>(
        &'a self,
        id: &'a UpdateId,
        options: &'a ApplyOptions,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<UpdateOutcome, UpdateError>> + Send + 'a,
        >,
    >;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_id_round_trips() {
        let id = SourceId::new("plugins");
        let json = serde_json::to_string(&id).unwrap();
        let back: SourceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
        assert_eq!(json, "\"plugins\"");
    }

    #[test]
    fn update_id_round_trips() {
        let id = UpdateId::new("evo@0.1.13.1");
        let json = serde_json::to_string(&id).unwrap();
        let back: UpdateId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn update_severity_orders_routine_below_critical() {
        assert!(UpdateSeverity::Routine < UpdateSeverity::Recommended);
        assert!(UpdateSeverity::Recommended < UpdateSeverity::Security);
        assert!(UpdateSeverity::Security < UpdateSeverity::Critical);
    }

    #[test]
    fn update_severity_serialises_snake_case() {
        let s = serde_json::to_string(&UpdateSeverity::Security).unwrap();
        assert_eq!(s, "\"security\"");
    }

    #[test]
    fn restart_level_serialises_snake_case() {
        let s = serde_json::to_string(&RestartLevel::Steward).unwrap();
        assert_eq!(s, "\"steward\"");
    }

    #[test]
    fn update_available_round_trips() {
        let now = SystemTime::UNIX_EPOCH;
        let u = UpdateAvailable {
            id: UpdateId::new("evo@0.1.13.1"),
            component: "evo".into(),
            current_version: "0.1.13".into(),
            available_version: "0.1.13.1".into(),
            changelog_url: Some("https://example/changelog".to_string()),
            severity: UpdateSeverity::Security,
            size_bytes: Some(24_000_000),
            requires_restart: RestartLevel::Steward,
            published_at: now,
        };
        let json = serde_json::to_string(&u).unwrap();
        let back: UpdateAvailable = serde_json::from_str(&json).unwrap();
        assert_eq!(u, back);
    }

    #[test]
    fn apply_options_default_is_safe() {
        let opts = ApplyOptions::default();
        assert!(!opts.dry_run);
        assert_eq!(opts.approved_by, None);
    }
}
