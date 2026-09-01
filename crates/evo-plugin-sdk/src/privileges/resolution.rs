// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Capability-resolution map: the framework's admission-time answer
//! to every [`CapabilityIntent`] a plugin declares.
//!
//! The plugin's [`PrivilegesV1`] record describes what the plugin
//! needs. The framework's admission gate runs a [`Probe`] for every
//! intent (translation from declarative intent to executable probe
//! is the [`probe`](super::probe) module's job) and records each
//! intent's [`CapabilityResolution`] in a
//! [`CapabilityResolutionMap`]. The map is then handed to the
//! plugin via `LoadContext::capabilities` so the plugin's
//! `Plugin::load` body picks the right runtime strategy without
//! re-probing.
//!
//! The map exists because runtime probing of OS-level privilege is
//! both expensive (every probe is a syscall or process exec) and
//! lossy (probing too late lets the plugin's hot-path discover
//! "Failed: Interactive authentication required" instead of the
//! framework discovering it at admission). Centralising the probe
//! at admission gives the framework one place to enforce
//! enterprise-grade preflight and one place to surface
//! operator-actionable remediation.
//!
//! [`CapabilityIntent`]: crate::privileges::CapabilityIntent
//! [`PrivilegesV1`]: crate::privileges::PrivilegesV1
//! [`Probe`]: super::probe::Probe

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Resolution of one [`CapabilityIntent`] after the framework's
/// preflight ran the corresponding probe.
///
/// Three terminal states the framework records per intent, plus a
/// fourth `NotProbed` covering intents whose probe shape this build
/// does not understand (e.g. an OS-specific probe on the wrong
/// host). Plugins read the resolution map at load time and choose
/// their strategy:
///
/// - `Available` — install the production strategy.
/// - `Degraded` — install the fallback strategy named in
///   `fallback_strategy` (or `NoOp`-shape if the plugin's domain
///   accepts skip).
/// - `Unavailable` — if the intent's `failure_mode` declared
///   "required", refuse admission (return [`PluginError::Permanent`]
///   with the embedded remedy). Else, install `NoOp` and continue.
/// - `NotProbed` — treat as `Degraded` with a generic reason;
///   probe coverage on this OS is incomplete, not a host fault.
///
/// [`PluginError::Permanent`]: crate::contract::PluginError::Permanent
/// [`CapabilityIntent`]: crate::privileges::CapabilityIntent
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CapabilityResolution {
    /// Probe succeeded; the plugin's declared intent is honoured by
    /// the runtime environment. `evidence` carries an
    /// operator-readable description of WHY the probe passed
    /// (e.g. `"sudo -l shows /usr/bin/systemctl restart mpd is
    /// allowed for the current user"`).
    Available {
        /// Operator-readable description of probe-positive evidence.
        evidence: String,
        /// Optional structured strategy hint. Set by probes that
        /// know which production strategy to install (e.g.
        /// `"sudo"` vs `"direct"` for the MPD restart leg).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strategy: Option<String>,
    },
    /// Probe ran and returned negative. Capability is not
    /// satisfied. `remedy` is the operator-actionable next step —
    /// typically the exact bootstrap command or sudoers fragment
    /// to install.
    Unavailable {
        /// Operator-readable description of WHY the probe failed.
        reason: String,
        /// Operator-actionable remediation step. Must be
        /// concrete enough that an operator can execute it
        /// verbatim (e.g. `"run /opt/evo/scripts/bootstrap.sh
        /// --install-mpd-sudoers"`).
        remedy: String,
    },
    /// Probe succeeded for a fallback strategy but not for the
    /// preferred one. Plugin admits with the fallback installed.
    /// `fallback_strategy` carries the operator-readable name of
    /// the strategy the framework selected.
    Degraded {
        /// Operator-readable description of why the preferred
        /// strategy was unavailable.
        reason: String,
        /// Strategy name the plugin should install instead
        /// (e.g. `"no_op"` to skip the leg, `"sudo"` to use sudo
        /// when direct exec was preferred).
        fallback_strategy: String,
    },
    /// Probe was not run on this host. Typically because the
    /// probe shape is OS-specific and this OS is out of scope
    /// for the running build. Plugin treats as `Degraded` with
    /// a generic operator-readable reason.
    NotProbed {
        /// Operator-readable description of why the probe was
        /// skipped (e.g. `"sudoers probes are Linux-only;
        /// running on macos"`).
        reason: String,
    },
}

impl CapabilityResolution {
    /// True when the resolution is `Available`. Convenience
    /// shorthand for the common load-time gate.
    pub fn is_available(&self) -> bool {
        matches!(self, CapabilityResolution::Available { .. })
    }

    /// True when the resolution is `Unavailable`. Plugins whose
    /// intent declared `failure_mode: required` use this to
    /// refuse admission with the embedded `remedy`.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, CapabilityResolution::Unavailable { .. })
    }

    /// Operator-readable one-line summary of the resolution.
    /// Used by the framework's admission-report logging and by
    /// `evo-plugin-tool privileges check` output.
    pub fn summary(&self) -> String {
        match self {
            CapabilityResolution::Available { evidence, strategy } => {
                match strategy {
                    Some(s) => format!("available (strategy={s}): {evidence}"),
                    None => format!("available: {evidence}"),
                }
            }
            CapabilityResolution::Unavailable { reason, remedy } => {
                format!("unavailable: {reason}; remedy: {remedy}")
            }
            CapabilityResolution::Degraded {
                reason,
                fallback_strategy,
            } => format!("degraded (fallback={fallback_strategy}): {reason}"),
            CapabilityResolution::NotProbed { reason } => {
                format!("not probed: {reason}")
            }
        }
    }
}

/// Map from [`CapabilityIntent`] id to its
/// [`CapabilityResolution`]. The plugin's `Plugin::load` reads
/// this via `LoadContext::capabilities`.
///
/// Ordered (`BTreeMap`) so the framework's admission-report
/// logging is stable across runs.
///
/// [`CapabilityIntent`]: crate::privileges::CapabilityIntent
pub type CapabilityResolutionMap = BTreeMap<String, CapabilityResolution>;

/// Convenience helpers for callers that build the resolution map
/// programmatically (the framework's admission code path, tests).
pub trait CapabilityResolutionMapExt {
    /// Look up an intent's resolution. Returns
    /// `CapabilityResolution::NotProbed { reason: "intent not in
    /// map" }` when absent so call sites get a uniform shape
    /// without panic.
    fn lookup(&self, intent_id: &str) -> CapabilityResolution;

    /// Count of intents in each terminal state. Used by the
    /// framework's admission-report summary line.
    fn counts(&self) -> ResolutionCounts;
}

impl CapabilityResolutionMapExt for CapabilityResolutionMap {
    fn lookup(&self, intent_id: &str) -> CapabilityResolution {
        self.get(intent_id).cloned().unwrap_or_else(|| {
            CapabilityResolution::NotProbed {
                reason: format!(
                    "intent {intent_id:?} not present in resolution map"
                ),
            }
        })
    }

    fn counts(&self) -> ResolutionCounts {
        let mut counts = ResolutionCounts::default();
        for resolution in self.values() {
            match resolution {
                CapabilityResolution::Available { .. } => counts.available += 1,
                CapabilityResolution::Unavailable { .. } => {
                    counts.unavailable += 1
                }
                CapabilityResolution::Degraded { .. } => counts.degraded += 1,
                CapabilityResolution::NotProbed { .. } => {
                    counts.not_probed += 1
                }
            }
        }
        counts
    }
}

/// Aggregated counts of resolutions in each terminal state, for
/// admission-report summary lines.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionCounts {
    /// Count of `Available` resolutions.
    pub available: usize,
    /// Count of `Unavailable` resolutions.
    pub unavailable: usize,
    /// Count of `Degraded` resolutions.
    pub degraded: usize,
    /// Count of `NotProbed` resolutions.
    pub not_probed: usize,
}

impl ResolutionCounts {
    /// Total resolutions across all states.
    pub fn total(&self) -> usize {
        self.available + self.unavailable + self.degraded + self.not_probed
    }

    /// True when every intent resolved to `Available`. The
    /// strongest admission posture: every declared capability is
    /// honoured by the host.
    pub fn all_available(&self) -> bool {
        self.unavailable == 0 && self.degraded == 0 && self.not_probed == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_is_available_reports_true_for_available_variant_only() {
        let r = CapabilityResolution::Available {
            evidence: "probe passed".into(),
            strategy: None,
        };
        assert!(r.is_available());
        assert!(!r.is_unavailable());

        let r = CapabilityResolution::Unavailable {
            reason: "x".into(),
            remedy: "y".into(),
        };
        assert!(!r.is_available());
        assert!(r.is_unavailable());

        let r = CapabilityResolution::Degraded {
            reason: "x".into(),
            fallback_strategy: "no_op".into(),
        };
        assert!(!r.is_available());
        assert!(!r.is_unavailable());

        let r = CapabilityResolution::NotProbed { reason: "x".into() };
        assert!(!r.is_available());
        assert!(!r.is_unavailable());
    }

    #[test]
    fn resolution_summary_renders_each_variant() {
        let r = CapabilityResolution::Available {
            evidence: "sudo -l shows it".into(),
            strategy: Some("sudo".into()),
        };
        assert_eq!(r.summary(), "available (strategy=sudo): sudo -l shows it");

        let r = CapabilityResolution::Available {
            evidence: "exec succeeded as root".into(),
            strategy: None,
        };
        assert_eq!(r.summary(), "available: exec succeeded as root");

        let r = CapabilityResolution::Unavailable {
            reason: "sudoers drop-in missing".into(),
            remedy: "run bootstrap".into(),
        };
        assert_eq!(
            r.summary(),
            "unavailable: sudoers drop-in missing; remedy: run bootstrap"
        );

        let r = CapabilityResolution::Degraded {
            reason: "direct exec refused".into(),
            fallback_strategy: "sudo".into(),
        };
        assert_eq!(
            r.summary(),
            "degraded (fallback=sudo): direct exec refused"
        );

        let r = CapabilityResolution::NotProbed {
            reason: "non-linux host".into(),
        };
        assert_eq!(r.summary(), "not probed: non-linux host");
    }

    #[test]
    fn map_lookup_absent_returns_not_probed() {
        let map: CapabilityResolutionMap = BTreeMap::new();
        let r = map.lookup("missing_intent");
        match r {
            CapabilityResolution::NotProbed { reason } => {
                assert!(reason.contains("missing_intent"));
            }
            other => panic!("expected NotProbed, got {other:?}"),
        }
    }

    #[test]
    fn map_counts_aggregates_each_state() {
        let mut map = CapabilityResolutionMap::new();
        map.insert(
            "a".into(),
            CapabilityResolution::Available {
                evidence: "ok".into(),
                strategy: None,
            },
        );
        map.insert(
            "b".into(),
            CapabilityResolution::Available {
                evidence: "ok".into(),
                strategy: Some("direct".into()),
            },
        );
        map.insert(
            "c".into(),
            CapabilityResolution::Unavailable {
                reason: "x".into(),
                remedy: "y".into(),
            },
        );
        map.insert(
            "d".into(),
            CapabilityResolution::Degraded {
                reason: "x".into(),
                fallback_strategy: "no_op".into(),
            },
        );
        map.insert(
            "e".into(),
            CapabilityResolution::NotProbed { reason: "x".into() },
        );

        let counts = map.counts();
        assert_eq!(counts.available, 2);
        assert_eq!(counts.unavailable, 1);
        assert_eq!(counts.degraded, 1);
        assert_eq!(counts.not_probed, 1);
        assert_eq!(counts.total(), 5);
        assert!(!counts.all_available());
    }

    #[test]
    fn map_counts_all_available_when_every_intent_passes() {
        let mut map = CapabilityResolutionMap::new();
        map.insert(
            "a".into(),
            CapabilityResolution::Available {
                evidence: "ok".into(),
                strategy: None,
            },
        );
        map.insert(
            "b".into(),
            CapabilityResolution::Available {
                evidence: "ok".into(),
                strategy: None,
            },
        );
        let counts = map.counts();
        assert!(counts.all_available());
    }

    #[test]
    fn resolution_serde_round_trips() {
        let original = CapabilityResolution::Available {
            evidence: "sudo dry-run succeeded".into(),
            strategy: Some("sudo".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: CapabilityResolution =
            serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);

        let original = CapabilityResolution::Unavailable {
            reason: "x".into(),
            remedy: "y".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: CapabilityResolution =
            serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn resolution_status_tag_is_snake_case() {
        let r = CapabilityResolution::NotProbed { reason: "x".into() };
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            json.contains("\"status\":\"not_probed\""),
            "expected snake_case tag, got: {json}"
        );
    }
}
