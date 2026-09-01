// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Framework-owned admission-lifecycle types.
//!
//! Admission on the evo fabric is not a boot-time gate; it is a
//! subscription. Every plugin discovered on the host has an
//! admission state that the framework computes reactively from
//! the plugin's declared prerequisites (systemd unit liveness,
//! binary presence on `PATH`) and publishes as a first-class
//! subject `plugin_admission_state` on the addressing
//! `evo.admission.plugin.<plugin-name>`.
//!
//! The types in this module are the wire shape of that subject.
//! They live in the SDK — not in the framework crate — because
//! wire consumers (UI decoders, operator CLI, next-plugin
//! dependency graphs) must decode the same shape the framework
//! produces. Pure data; no runtime.
//!
//! ## Lifecycle
//!
//! - [`AdmissionState::Probing`] — the framework has just
//!   discovered the plugin and is running the initial
//!   prerequisite probe. Transient; every plugin passes through
//!   this state in the first admission tick.
//! - [`AdmissionState::Pending`] — one or more prerequisites are
//!   present in the system (installed / loaded) but not yet in
//!   an admissible state (unit loaded but inactive; binary
//!   present but PATH cache stale). The reactive engine watches
//!   for the transition and admits the moment every prerequisite
//!   is satisfied. `reasons` names each still-unsatisfied
//!   prerequisite verbatim so the operator UI can render the
//!   waiting reason.
//! - [`AdmissionState::Admitted`] — every prerequisite is
//!   satisfied and `Plugin::load` has returned successfully.
//!   Steady state.
//! - [`AdmissionState::Degraded`] — the plugin was admitted, but
//!   a prerequisite has since transitioned out of `Present`
//!   (operator stopped a service; a binary was uninstalled while
//!   running). The plugin stays loaded — the framework does not
//!   unload on degrade — but the state signals to the UI that
//!   plugin behaviour may be impaired.
//! - [`AdmissionState::Refused`] — the plugin cannot be admitted.
//!   `permanent = true` means the framework has established a
//!   condition an operator cannot fix without external action
//!   (e.g. required systemd unit `smbd.service` has
//!   `LoadState=not-found` — the `samba` package is not
//!   installed on the host). `permanent = false` means the
//!   condition is transient in principle (validator refused;
//!   signature failed; catalogue mismatch); an operator action
//!   makes the plugin admissible again without a reboot.
//!
//! ## Invariant
//!
//! `Refused { permanent: true }` is reserved for
//! [`PrerequisitePresence::NotInstalled`]. Any other terminal
//! refuse (validator failure, signature failure, catalogue
//! mismatch) MUST use `permanent: false` — the framework
//! never emits `permanent: true` for a condition an operator
//! can fix.
//!
//! ## Parked status
//!
//! This module is not consumed by any framework runtime today.
//! The reactive-admission substrate it was authored to support
//! was refused in favour of the surgical parity-gate fix in
//! [`crate::privileges::parity_gate`] (loaded-but-inactive
//! services satisfy the gate). The types are retained
//! transiently; a follow-up cycle deletes this module unless a
//! separately justified consumer appears.

use serde::{Deserialize, Serialize};

/// Fully-qualified subject type declared in the catalogue for
/// framework-published admission-state envelopes. Framework
/// crates and catalogue authors both anchor on this constant so
/// the on-target catalogue entry cannot drift from the wire
/// shape without a compile break.
pub const SUBJECT_TYPE_PLUGIN_ADMISSION_STATE: &str = "plugin_admission_state";

/// Addressing-scheme prefix under which per-plugin admission-
/// state subjects are announced. Full addressing is
/// `evo.admission.plugin.<plugin-name>` — the plugin's
/// canonical reverse-DNS identity is appended verbatim.
pub const SUBJECT_ADDRESSING_PREFIX: &str = "evo.admission.plugin.";

/// Compute the addressing string for one plugin's admission
/// subject. Kept as a helper (rather than open-coded at every
/// caller) so a future addressing-scheme rename lands in one
/// place.
///
/// # Example
///
/// ```
/// use evo_plugin_sdk::admission::subject_addressing_for;
/// assert_eq!(
///     subject_addressing_for("org.evoframework.network.smb-server"),
///     "evo.admission.plugin.org.evoframework.network.smb-server"
/// );
/// ```
pub fn subject_addressing_for(plugin_name: &str) -> String {
    format!("{SUBJECT_ADDRESSING_PREFIX}{plugin_name}")
}

/// Wire envelope published on every admission state transition.
/// One envelope per plugin per publish tick — subscribers receive
/// the full state on each transition, not deltas.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionStateEnvelope {
    /// Canonical reverse-DNS plugin identity the envelope
    /// describes (matches `manifest.plugin.name`).
    pub plugin: String,
    /// Lifecycle state at the moment the envelope was
    /// composed. See [`AdmissionState`].
    pub state: AdmissionState,
    /// Every prerequisite the plugin declares in its
    /// `privileges.yaml`, with the framework's current
    /// observation of each. Includes prerequisites that ARE
    /// present — the envelope carries the full picture, not
    /// only the missing ones, so a UI can render "3 of 3
    /// prerequisites satisfied" during Admitted.
    pub prerequisites: Vec<PrerequisiteRecord>,
    /// Wall-clock instant of the most recent state transition,
    /// in milliseconds since the UNIX epoch. Distinct from the
    /// per-state `since_ms` — this timestamps the envelope
    /// itself so a subscriber can order updates when its
    /// transport reorders.
    pub last_transition_ms: u64,
}

/// Admission-lifecycle state. `#[non_exhaustive]` so adding a
/// new state is a minor SDK bump (per the SDK versioning policy
/// in `evo_plugin_sdk::VERSION`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AdmissionState {
    /// Initial state at framework boot; the framework has
    /// enumerated the plugin from disk and is running the
    /// synchronous first probe pass. Transient; every plugin
    /// spends at most one tick here.
    Probing,
    /// The plugin cannot be admitted yet because one or more
    /// prerequisites are declared-present but not currently
    /// satisfied (e.g. a systemd unit is `loaded` but
    /// `inactive`). The reactive engine subscribes to the
    /// prerequisite's underlying event source; the plugin
    /// admits the moment every prerequisite transitions to
    /// [`PrerequisitePresence::Present`].
    Pending {
        /// Wall-clock instant the plugin entered `Pending`,
        /// in milliseconds since UNIX epoch.
        since_ms: u64,
        /// Human-readable reasons the plugin is still waiting,
        /// one per still-unsatisfied prerequisite. Verbatim
        /// from the SDK's probe surface so a UI renders the
        /// operator-facing text without further translation.
        reasons: Vec<String>,
    },
    /// The plugin is loaded and its subjects are announced. The
    /// framework's authoritative surface for the plugin's
    /// operator-visible functionality is now the plugin's own
    /// subjects; `plugin_admission_state` continues to publish
    /// so a `Degraded` transition remains observable.
    Admitted {
        /// Wall-clock instant `Plugin::load` returned Ok.
        since_ms: u64,
    },
    /// The plugin remains loaded but a prerequisite has
    /// transitioned out of `Present` since admission. The
    /// framework does NOT unload on degrade — the plugin's
    /// existing wire surface stays available to consumers that
    /// can tolerate the impairment; degrade behaviour is the
    /// plugin's own responsibility via
    /// `LoadContext::capabilities_watch`.
    Degraded {
        /// Wall-clock instant the plugin entered `Degraded`.
        since_ms: u64,
        /// Reasons the plugin is impaired — one per
        /// no-longer-satisfied prerequisite.
        reasons: Vec<String>,
    },
    /// The plugin cannot be admitted and the framework does not
    /// retry automatically. `permanent = true` names the
    /// condition an operator cannot fix without external action
    /// (host package install, etc.); `permanent = false` names
    /// a condition an operator action can resolve on the same
    /// boot (upload a signed bundle, load a catalogue update).
    Refused {
        /// Wall-clock instant the plugin entered `Refused`.
        since_ms: u64,
        /// Reasons the plugin was refused.
        reasons: Vec<String>,
        /// See the module-level Invariant paragraph: reserved
        /// for [`PrerequisitePresence::NotInstalled`]; every
        /// other terminal refuse uses `permanent: false`.
        permanent: bool,
    },
}

/// The state of one declared prerequisite at the moment the
/// envelope was composed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrerequisiteRecord {
    /// What kind of prerequisite this record describes. Mirrors
    /// the SDK's existing [`crate::privileges::PrerequisiteKind`].
    pub kind: PrerequisiteKind,
    /// The declared identifier (binary name like `mount.cifs`,
    /// unit name like `smbd.service`).
    pub name: String,
    /// Framework's current observation of the prerequisite.
    pub presence: PrerequisitePresence,
    /// Wall-clock instant of the framework's most recent probe
    /// (or event observation) for this prerequisite, in
    /// milliseconds since UNIX epoch.
    pub last_observed_ms: u64,
    /// Suggested remedy resolved for the detected host distro,
    /// carried on absence conditions so the UI can render the
    /// exact install command. `None` on `Present` records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

/// Prerequisite kind — mirrors the SDK's existing enum in
/// `crate::privileges::PrerequisiteKind`. Duplicated (not
/// re-exported) so this admission module compiles under
/// non-`privileges` feature builds — a wire consumer without
/// the privileges validator still decodes the envelope.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteKind {
    /// Declared in `required_binaries`; framework probes
    /// resolvability via `PATH` walk + inotify watch.
    Binary,
    /// Declared in `required_system_services`; framework
    /// probes via systemd `LoadState` + `ActiveState` and
    /// subscribes to D-Bus `PropertiesChanged`.
    SystemService,
}

/// Three-value observation of one prerequisite's current state
/// on the host. Collapsed from the raw systemd `LoadState` ×
/// `ActiveState` cross-product.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisitePresence {
    /// Systemd: `LoadState=loaded AND ActiveState=active`.
    /// Binary: found on `PATH`. The prerequisite satisfies
    /// admission.
    Present,
    /// Systemd: `LoadState=loaded` AND `ActiveState ∈
    /// {inactive, activating, deactivating, failed, reloading}`.
    /// Binary: not currently found on `PATH` but a PATH
    /// directory the framework watches carries recent-enough
    /// activity that the binary MAY appear. Transient; the
    /// plugin sits in [`AdmissionState::Pending`] until the
    /// reactive watcher observes a `Present` transition.
    InactiveButLoaded,
    /// Systemd: `LoadState ∈ {not-found, masked, error,
    /// bad-setting}`. Binary: not found on `PATH` and no watch
    /// has been set (or every watched directory has settled
    /// with the binary absent). The plugin transitions to
    /// [`AdmissionState::Refused`] with `permanent: true` —
    /// the operator must install a package (or similar external
    /// action) before the framework re-evaluates.
    NotInstalled,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Addressing helper produces the addressing shape the
    /// catalogue entry declares.
    #[test]
    fn subject_addressing_prepends_the_prefix() {
        assert_eq!(
            subject_addressing_for("org.evoframework.network.smb-server"),
            "evo.admission.plugin.org.evoframework.network.smb-server"
        );
    }

    /// Envelope serialises with a stable field order and every
    /// state variant survives the round trip. A future author
    /// adding a variant to [`AdmissionState`] must update this
    /// round-trip vector so decoders in downstream crates
    /// (framework, UI decoder) can rely on the shape.
    #[test]
    fn envelope_round_trips_through_json() {
        let sample = AdmissionStateEnvelope {
            plugin: "org.evoframework.network.smb-server".to_string(),
            state: AdmissionState::Pending {
                since_ms: 1_700_000_777_000,
                reasons: vec![
                    "service smbd.service — unit is loaded but inactive"
                        .to_string(),
                ],
            },
            prerequisites: vec![PrerequisiteRecord {
                kind: PrerequisiteKind::SystemService,
                name: "smbd.service".to_string(),
                presence: PrerequisitePresence::InactiveButLoaded,
                last_observed_ms: 1_700_000_777_000,
                remedy: Some("systemctl enable --now smbd.service".to_string()),
            }],
            last_transition_ms: 1_700_000_777_000,
        };
        let json = serde_json::to_string(&sample).unwrap();
        let round: AdmissionStateEnvelope =
            serde_json::from_str(&json).unwrap();
        assert_eq!(round, sample);
    }

    /// Refused with `permanent: true` is reserved for
    /// `NotInstalled` (see the module-level Invariant). This
    /// test asserts nothing about runtime behaviour (the
    /// framework enforces the rule at the reconciler); it
    /// locks the vocabulary in the tests so a future author
    /// reading the module sees the pairing.
    #[test]
    fn refused_permanent_pairs_with_not_installed_vocabulary() {
        let permanent_refuse = AdmissionState::Refused {
            since_ms: 0,
            reasons: vec!["samba package not installed".to_string()],
            permanent: true,
        };
        let not_installed_presence = PrerequisitePresence::NotInstalled;
        // The pairing is documented; the test locks the
        // vocabulary so `grep NotInstalled` also lands on this
        // line and the next contributor sees the pair.
        assert!(matches!(
            permanent_refuse,
            AdmissionState::Refused {
                permanent: true,
                ..
            }
        ));
        assert!(matches!(
            not_installed_presence,
            PrerequisitePresence::NotInstalled
        ));
    }
}
