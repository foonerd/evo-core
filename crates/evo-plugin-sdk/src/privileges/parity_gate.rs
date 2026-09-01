// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Compulsory install-time and admission-time parity gate.
//!
//! Every install path (`evo-plugin-tool install`, distribution
//! installer, drop-in steward admission) MUST call
//! [`enforce_os_dependency_parity`] before promoting or admitting
//! a plugin bundle. The gate reads `has_os_dependencies` from the
//! plugin's `privileges.yaml`; when `true`, every declared
//! `required_binaries` entry must resolve on host `PATH` AND every
//! declared `required_system_services` entry must be
//! systemd-**reachable** (`LoadState=loaded`). First miss = HARD
//! FAIL with a structured [`ParityFailure`] naming the missing
//! prerequisite and its remediation.
//!
//! Reachable ≠ running. A unit that is installed but currently
//! `inactive` / `activating` / `failed` still satisfies the gate
//! — ActiveState is a runtime concern for the plugin (health,
//! apply paths), not an admission/install refuse. Requiring
//! Active at this gate caused cold-boot races: the steward
//! admitted before peer units finished starting, then refused
//! the plugin for the rest of the boot. The gate's contract
//! has always been "reachable" (the unit file resolves on
//! disk, LoadState=loaded); this module implements that bar
//! via [`probe_systemd_unit_state`].
//!
//! There is no "warn" mode. There is no "degrade" mode. There is
//! no bypass flag. `has_os_dependencies=false` is the only path
//! that skips the check, and the validator enforces that `false`
//! is truthful (any non-empty `required_*` vector blocks
//! validation).

use std::process::Command;

use serde::{Deserialize, Serialize};

use super::probe::BinaryPresentProbe;
use super::probe::{Probe, ProbeOutcome};
use super::remediation::{
    hint_for_missing_binary, hint_for_missing_service, DistroFamily,
    RemediationHint,
};
use super::types::PrivilegesV1;

/// What kind of prerequisite the parity gate refused on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteKind {
    /// Declared in `required_binaries`; not resolvable on PATH.
    Binary,
    /// Declared in `required_system_services`; unit is not
    /// resolvable by systemd (`LoadState` not-found / masked /
    /// error / bad-setting), or the probe itself failed.
    SystemService,
}

/// One missing prerequisite surfaced by the parity gate. Rendered
/// into the install / admission failure message so the operator
/// sees exactly what is missing and how to install it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingPrerequisite {
    /// What kind of prerequisite this is (binary vs system service).
    pub kind: PrerequisiteKind,
    /// The declared identifier (binary name like `mount.cifs`, or
    /// unit name like `avahi-daemon.service`).
    pub name: String,
    /// Operator-readable reason (from the probe outcome).
    pub reason: String,
    /// Suggested remediation resolved for the detected distro.
    /// For binaries: typically a `PackageInstall` hint. For
    /// services: an enable+start hint.
    pub remediation: RemediationHint,
}

/// Structured failure returned when the parity gate refuses. The
/// caller (install tool / steward admission) surfaces this as the
/// operator-facing error verbatim; the machine-readable structure
/// is preserved for the diagnostics wire op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParityFailure {
    /// Plugin identifier from the record.
    pub plugin: String,
    /// Distribution family detected on the host, or Unknown.
    pub distro: DistroFamily,
    /// Every missing prerequisite. Non-empty (the gate only
    /// returns `Err` when there is at least one miss).
    pub missing: Vec<MissingPrerequisite>,
}

impl std::fmt::Display for ParityFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "plugin `{}` install refused: has_os_dependencies=true \
             but {} declared prerequisite(s) are absent on the host \
             (distro={}):",
            self.plugin,
            self.missing.len(),
            self.distro.as_str()
        )?;
        for m in &self.missing {
            let kind = match m.kind {
                PrerequisiteKind::Binary => "binary",
                PrerequisiteKind::SystemService => "service",
            };
            writeln!(
                f,
                "  - {kind} {} — {}\n      remedy: {}",
                m.name,
                m.reason,
                m.remediation.shell_command()
            )?;
        }
        write!(
            f,
            "the distribution installer must bring these packages / \
             enable these services before install; refusing to promote a bundle \
             whose declared runtime prerequisites are unmet"
        )
    }
}

impl std::error::Error for ParityFailure {}

/// Compulsory install/admission-time parity check.
///
/// Contract:
/// - `record.has_os_dependencies == false` → `Ok(())` (validator
///   already proved every `required_*` vector is empty).
/// - `record.has_os_dependencies == true` → every declared
///   `required_binary.name` MUST resolve via
///   [`BinaryPresentProbe`], and every declared
///   `required_system_services` unit MUST be systemd-reachable
///   (`LoadState=loaded`). ActiveState is **not** gated here —
///   loaded-but-inactive passes. Missing entries collect into a
///   [`ParityFailure`]; any miss returns `Err`.
///
/// Distro is detected via [`DistroFamily::detect_local`]; the
/// remediation hint uses that detection to name the package that
/// provides each missing binary. Detection failure yields
/// `DistroFamily::Unknown` and the remediation renders as a
/// `Custom` hint asking the operator to install via the local
/// package manager.
pub fn enforce_os_dependency_parity(
    record: &PrivilegesV1,
) -> Result<(), ParityFailure> {
    if !record.has_os_dependencies {
        return Ok(());
    }
    let distro = DistroFamily::detect_local();
    let mut missing: Vec<MissingPrerequisite> = Vec::new();
    for bin in &record.required_binaries {
        let probe = BinaryPresentProbe::new(&bin.name);
        match probe.run() {
            ProbeOutcome::Satisfied { .. } => {}
            ProbeOutcome::Unsatisfied { reason }
            | ProbeOutcome::ProbeError { diagnostic: reason }
            | ProbeOutcome::InapplicableOnThisOs { reason } => {
                missing.push(MissingPrerequisite {
                    kind: PrerequisiteKind::Binary,
                    name: bin.name.clone(),
                    reason,
                    remediation: hint_for_missing_binary(&bin.name, distro),
                });
            }
        }
    }
    for unit in &record.required_system_services {
        let state = probe_systemd_unit_state(unit);
        if let Some(reason) = service_parity_refuse_reason(&state, unit) {
            missing.push(MissingPrerequisite {
                kind: PrerequisiteKind::SystemService,
                name: unit.clone(),
                reason,
                remediation: hint_for_missing_service(unit),
            });
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(ParityFailure {
        plugin: record.plugin.clone(),
        distro,
        missing,
    })
}

/// Whether a probed systemd unit fails the parity gate.
///
/// Passes [`SystemServiceState::Present`] and
/// [`SystemServiceState::InactiveButLoaded`] — the unit file is
/// on disk; runtime ActiveState is the plugin's concern.
/// Refuses [`SystemServiceState::NotInstalled`] and
/// [`SystemServiceState::ProbeError`].
pub fn service_parity_refuse_reason(
    state: &SystemServiceState,
    unit: &str,
) -> Option<String> {
    match state {
        SystemServiceState::Present
        | SystemServiceState::InactiveButLoaded { .. } => None,
        SystemServiceState::NotInstalled { .. }
        | SystemServiceState::ProbeError { .. } => state.reason(unit),
    }
}

/// Observation of one systemd unit via a single
/// `systemctl show --property=LoadState,ActiveState` call.
/// Distinguishes "installed but not yet up"
/// ([`SystemServiceState::InactiveButLoaded`]) from "not
/// installed" ([`SystemServiceState::NotInstalled`]) so the
/// parity gate can refuse only the latter.
///
/// Non-Linux hosts return [`SystemServiceState::Present`]
/// (systemd is Linux-specific; macOS / Windows dev boxes that
/// never run a steward with `required_system_services` declared
/// must not trip the gate).
pub fn probe_systemd_unit_state(unit: &str) -> SystemServiceState {
    #[cfg(target_os = "linux")]
    {
        // One `systemctl show` reads both properties atomically
        // so the pair is coherent (avoiding the case where a
        // unit transitions between two separate probe calls).
        //
        // `--value` prints just the property values one per
        // line in the order requested, with no `PropertyName=`
        // prefix — the caller reads them positionally.
        let output = match Command::new("systemctl")
            .arg("show")
            .arg("--property=LoadState,ActiveState")
            .arg("--value")
            .arg(unit)
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                return SystemServiceState::ProbeError {
                    reason: format!("systemctl invocation failed: {e}"),
                };
            }
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        let load_state = lines.next().unwrap_or("").trim();
        let active_state = lines.next().unwrap_or("").trim();
        match load_state {
            "loaded" => match active_state {
                "active" => SystemServiceState::Present,
                // Loaded but not active — still satisfies the
                // parity gate (reachable). ActiveState is the
                // plugin's runtime concern.
                other => SystemServiceState::InactiveButLoaded {
                    active_state: other.to_string(),
                },
            },
            // Unit file not resolvable on disk — package missing,
            // masked, or broken. Parity refuses.
            "not-found" | "masked" | "error" | "bad-setting" => {
                SystemServiceState::NotInstalled {
                    load_state: load_state.to_string(),
                }
            }
            // Empty LoadState (systemctl returned nothing —
            // typo, or systemd not running). Refuse with the
            // diagnostic rather than silently treating as present.
            "" => SystemServiceState::ProbeError {
                reason: format!(
                    "systemctl returned empty LoadState for `{unit}`; \
                     systemd may not be running"
                ),
            },
            unknown => SystemServiceState::ProbeError {
                reason: format!(
                    "unit `{unit}` returned unrecognised LoadState \
                     `{unknown}` (expected one of loaded / not-found / \
                     masked / error / bad-setting)"
                ),
            },
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = unit;
        SystemServiceState::Present
    }
}

/// Systemd-unit observation from [`probe_systemd_unit_state`].
/// The parity gate collapses this via
/// [`service_parity_refuse_reason`]: loaded (active or not)
/// passes; not-resolvable / probe-error refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemServiceState {
    /// Unit is loaded AND active.
    Present,
    /// Unit is loaded (unit file on disk) but not currently
    /// active. Satisfies the parity gate; runtime ActiveState
    /// remains the plugin's concern.
    InactiveButLoaded {
        /// Raw `ActiveState`: `inactive`, `activating`,
        /// `deactivating`, `failed`, or `reloading`.
        active_state: String,
    },
    /// Unit is not resolvable — package not installed, unit
    /// masked, or unit file bad. Parity refuses.
    NotInstalled {
        /// Raw `LoadState`: `not-found`, `masked`, `error`,
        /// or `bad-setting`.
        load_state: String,
    },
    /// The probe itself failed (systemctl not runnable, systemd
    /// not running, unexpected output). Distinct from
    /// [`Self::NotInstalled`] — the framework cannot tell
    /// whether the unit exists. Parity refuses with the
    /// diagnostic.
    ProbeError {
        /// Human-readable diagnostic explaining what the probe
        /// tried and how it failed.
        reason: String,
    },
}

impl SystemServiceState {
    /// Operator-facing reason for a non-Present observation.
    /// `Present` returns `None`.
    pub fn reason(&self, unit: &str) -> Option<String> {
        match self {
            Self::Present => None,
            Self::InactiveButLoaded { active_state } => Some(format!(
                "service `{unit}` — unit is loaded but \
                 ActiveState={active_state}"
            )),
            Self::NotInstalled { load_state } => Some(format!(
                "service `{unit}` — unit is not resolvable \
                 (LoadState={load_state})"
            )),
            Self::ProbeError { reason } => {
                Some(format!("service `{unit}` — probe failed: {reason}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_with(has: bool, binaries: &[&str]) -> PrivilegesV1 {
        let mut lines = String::from(
            "schema_version: \"1.0\"\n\
             plugin: org.evoframework.example\n\
             owner: example\n\
             isolation: oop\n",
        );
        lines.push_str(&format!("has_os_dependencies: {has}\n"));
        lines.push_str("capability_intent: []\n");
        if binaries.is_empty() {
            lines.push_str("required_binaries: []\n");
        } else {
            lines.push_str("required_binaries:\n");
            for name in binaries {
                lines.push_str(&format!(
                    "  - name: {name}\n    failure_mode: refuse to start\n"
                ));
            }
        }
        lines.push_str(
            "required_kernel_modules: []\n\
             required_system_services: []\n\
             verification:\n  commands: [\"true\"]\n  expected: [\"ok\"]\n\
             host_provisioning: {}\n",
        );
        PrivilegesV1::from_yaml(&lines).unwrap()
    }

    #[test]
    fn parity_ok_when_flag_false() {
        let record = record_with(false, &[]);
        assert!(enforce_os_dependency_parity(&record).is_ok());
    }

    #[test]
    fn parity_ok_when_flag_true_and_binary_present() {
        // /bin/sh is universally present on any host this crate targets.
        let record = record_with(true, &["sh"]);
        assert!(enforce_os_dependency_parity(&record).is_ok());
    }

    #[test]
    fn parity_fails_when_flag_true_and_binary_absent() {
        let record = record_with(true, &["zz_definitely_not_a_real_binary_zz"]);
        let err = enforce_os_dependency_parity(&record).unwrap_err();
        assert_eq!(err.plugin, "org.evoframework.example");
        assert_eq!(err.missing.len(), 1);
        assert_eq!(err.missing[0].kind, PrerequisiteKind::Binary);
        assert_eq!(err.missing[0].name, "zz_definitely_not_a_real_binary_zz");
    }

    #[test]
    fn parity_reports_every_missing_binary_not_just_first() {
        let record = record_with(
            true,
            &["sh", "zz_missing_one_zz", "zz_missing_two_zz"],
        );
        let err = enforce_os_dependency_parity(&record).unwrap_err();
        assert_eq!(err.missing.len(), 2);
    }

    #[test]
    fn parity_failure_display_is_operator_readable() {
        let record = record_with(true, &["zz_missing_zz"]);
        let err = enforce_os_dependency_parity(&record).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("org.evoframework.example"));
        assert!(rendered.contains("has_os_dependencies=true"));
        assert!(rendered.contains("zz_missing_zz"));
    }

    /// Missing unit → NotInstalled → parity refuses.
    ///
    /// Guarded on `systemctl` being present on the host — CI
    /// runners in minimal containers do not carry systemd, and
    /// the probe returns `ProbeError { reason: "systemctl
    /// invocation failed: No such file or directory ..." }` in
    /// that environment. The intent of this test is the LoadState
    /// classification arm, not the availability of the invoker
    /// itself; when the invoker is missing we skip cleanly rather
    /// than assert a shape the substrate cannot reach.
    #[test]
    #[cfg(target_os = "linux")]
    fn systemd_unit_state_probes_missing_unit_as_not_installed() {
        if !host_has_systemctl() {
            eprintln!(
                "skip: `systemctl` not on PATH — this test needs systemd on \
                 the host to distinguish NotInstalled from a probe-failure"
            );
            return;
        }
        let state =
            probe_systemd_unit_state("zz-nonsense-unit-does-not-exist.service");
        assert!(
            matches!(state, SystemServiceState::NotInstalled { .. }),
            "expected NotInstalled, got {state:?}"
        );
        let reason = service_parity_refuse_reason(
            &state,
            "zz-nonsense-unit-does-not-exist.service",
        );
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("LoadState="));
    }

    /// Runtime-check whether `systemctl` is reachable on the
    /// host. Any successful exit code (including non-zero
    /// error codes — the binary answered) counts; an
    /// invocation error (`ErrorKind::NotFound`) means the
    /// binary is not on PATH and the probe-based tests below
    /// skip themselves cleanly.
    #[cfg(target_os = "linux")]
    fn host_has_systemctl() -> bool {
        Command::new("systemctl").arg("--version").output().is_ok()
    }

    /// Parity gate: loaded (active or inactive) passes;
    /// not-installed and probe-error refuse. This is the
    /// cold-boot contract — InactiveButLoaded must NOT refuse.
    #[test]
    fn service_parity_passes_loaded_inactive_refuses_absent() {
        assert!(service_parity_refuse_reason(
            &SystemServiceState::Present,
            "smbd.service"
        )
        .is_none());
        assert!(service_parity_refuse_reason(
            &SystemServiceState::InactiveButLoaded {
                active_state: "inactive".to_string(),
            },
            "smbd.service"
        )
        .is_none());
        assert!(service_parity_refuse_reason(
            &SystemServiceState::InactiveButLoaded {
                active_state: "activating".to_string(),
            },
            "smbd.service"
        )
        .is_none());
        let refuse_absent = service_parity_refuse_reason(
            &SystemServiceState::NotInstalled {
                load_state: "not-found".to_string(),
            },
            "smbd.service",
        );
        assert!(refuse_absent.is_some());
        assert!(refuse_absent.unwrap().contains("not resolvable"));
        assert!(service_parity_refuse_reason(
            &SystemServiceState::ProbeError {
                reason: "systemctl not runnable".to_string(),
            },
            "smbd.service"
        )
        .is_some());
    }
}
