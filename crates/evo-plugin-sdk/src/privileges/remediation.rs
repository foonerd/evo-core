// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Structured remediation hints for parity-check failures.
//!
//! When the admission-time parity gate fails (missing required
//! binary, missing kernel module, unknown system service, missing
//! supplementary group membership, absent polkit drop-in), the
//! framework does not just refuse. It produces a structured
//! [`RemediationHint`] the operator can render as an "Apply"
//! affordance in the diagnostics surface and dispatch to the
//! distribution-provided unit-installer.
//!
//! ## Substrate contract
//!
//! The hint is a **suggestion**, not authority. The framework
//! never applies a hint on its own — the operator approves per
//! item; the unit-installer executes; the framework re-probes
//! and either admits or surfaces the residual failure. Fail-
//! closed default preserved: nothing runs without explicit
//! operator consent.
//!
//! Hints are advisory in another sense too: the binary-to-package
//! lookup is heuristic. `alsa-utils` on Debian corresponds to
//! `alsa-utils` on Alpine but to `alsa-utils` (via `apk add`),
//! and `alsa-lib-utils` on Fedora — there is no canonical
//! cross-distro package name. This module ships a small,
//! curated table for the most common binaries an audio-focused
//! reference build needs; distributions can extend by adding a
//! per-distribution override table.
//!
//! ## Wire posture
//!
//! [`RemediationHint`] serialises via serde (JSON default) so it
//! can traverse the admission-engine wire without a shape
//! change on either end.

use serde::{Deserialize, Serialize};

/// Distributions the framework knows how to render remediation
/// hints for. Detected from `/etc/os-release`'s `ID=` field on
/// Linux; unknown on other platforms.
///
/// Extending this list means adding a `binary_to_package` arm
/// and enumerating the distro's package-manager idiom
/// (`apt install`, `dnf install`, `apk add`, `pacman -S`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistroFamily {
    /// Debian, Ubuntu, Raspberry Pi OS, and downstream derivatives.
    /// Package manager: `apt` (also `apt-get`).
    Debian,
    /// Fedora, RHEL, CentOS Stream, AlmaLinux, Rocky Linux.
    /// Package manager: `dnf` (also `yum` on older releases).
    Fedora,
    /// Alpine Linux (musl-libc appliance base).
    /// Package manager: `apk`.
    Alpine,
    /// Arch Linux + Manjaro. Package manager: `pacman`.
    Arch,
    /// Distribution not recognised. Package-install hints render
    /// as `Custom` with a description asking the operator to
    /// install the named binary via the local package manager.
    Unknown,
}

impl DistroFamily {
    /// Operator-readable name for the distribution family.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Debian => "debian",
            Self::Fedora => "fedora",
            Self::Alpine => "alpine",
            Self::Arch => "arch",
            Self::Unknown => "unknown",
        }
    }

    /// Detect the running distribution by reading
    /// `/etc/os-release`. Returns [`DistroFamily::Unknown`] when
    /// the file is unreadable, malformed, or names a distribution
    /// this build does not know.
    ///
    /// Non-Linux hosts return [`DistroFamily::Unknown`].
    pub fn detect_local() -> Self {
        #[cfg(target_os = "linux")]
        {
            match std::fs::read_to_string("/etc/os-release") {
                Ok(text) => parse_os_release_id(&text),
                Err(_) => Self::Unknown,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::Unknown
        }
    }

    /// Package-manager command idiom for this distribution
    /// (`apt install`, `dnf install`, `apk add`, `pacman -S`).
    /// `Unknown` returns an empty string; callers should render
    /// as a `Custom` hint in that case.
    pub fn install_command(&self) -> &'static str {
        match self {
            Self::Debian => "apt install",
            Self::Fedora => "dnf install",
            Self::Alpine => "apk add",
            Self::Arch => "pacman -S",
            Self::Unknown => "",
        }
    }

    /// Resolve a binary name to its package on this
    /// distribution. Returns `None` for binaries this build has
    /// no entry for; callers can either fall back to the binary
    /// name (many packages ship a same-named binary) or emit a
    /// `Custom` hint.
    ///
    /// Curated table — extend as new audio-domain reference
    /// builds surface additional binaries.
    pub fn binary_to_package(&self, binary: &str) -> Option<&'static str> {
        match (self, binary) {
            // Audio-domain reference binaries (aligned with
            // the plugins the audio-reference distribution
            // ships). Extend as new required_binaries surface.
            (Self::Debian, "aplay") => Some("alsa-utils"),
            (Self::Debian, "amixer") => Some("alsa-utils"),
            (Self::Debian, "mpc") => Some("mpc"),
            (Self::Debian, "mpd") => Some("mpd"),
            (Self::Debian, "curl") => Some("curl"),
            (Self::Debian, "ffmpeg") => Some("ffmpeg"),
            (Self::Debian, "systemctl") => Some("systemd"),
            (Self::Debian, "polkit") => Some("policykit-1"),
            (Self::Debian, "nmcli") => Some("network-manager"),
            (Self::Debian, "smbclient") => Some("smbclient"),
            (Self::Debian, "mount.cifs") => Some("cifs-utils"),
            (Self::Debian, "mount.nfs") => Some("nfs-common"),
            (Self::Debian, "avahi-browse") => Some("avahi-utils"),
            (Self::Debian, "smbd") => Some("samba"),
            (Self::Debian, "testparm") => Some("samba-common-bin"),
            (Self::Debian, "smbpasswd") => Some("samba-common-bin"),

            (Self::Fedora, "aplay") => Some("alsa-utils"),
            (Self::Fedora, "amixer") => Some("alsa-utils"),
            (Self::Fedora, "mpc") => Some("mpc"),
            (Self::Fedora, "mpd") => Some("mpd"),
            (Self::Fedora, "curl") => Some("curl"),
            (Self::Fedora, "ffmpeg") => Some("ffmpeg"),
            (Self::Fedora, "systemctl") => Some("systemd"),
            (Self::Fedora, "polkit") => Some("polkit"),
            (Self::Fedora, "nmcli") => Some("NetworkManager"),
            (Self::Fedora, "smbclient") => Some("samba-client"),
            (Self::Fedora, "mount.cifs") => Some("cifs-utils"),
            (Self::Fedora, "mount.nfs") => Some("nfs-utils"),
            (Self::Fedora, "avahi-browse") => Some("avahi-tools"),
            (Self::Fedora, "smbd") => Some("samba"),
            (Self::Fedora, "testparm") => Some("samba-common-tools"),
            (Self::Fedora, "smbpasswd") => Some("samba-common-tools"),

            (Self::Alpine, "aplay") => Some("alsa-utils"),
            (Self::Alpine, "amixer") => Some("alsa-utils"),
            (Self::Alpine, "mpc") => Some("mpc"),
            (Self::Alpine, "mpd") => Some("mpd"),
            (Self::Alpine, "curl") => Some("curl"),
            (Self::Alpine, "ffmpeg") => Some("ffmpeg"),
            (Self::Alpine, "systemctl") => Some("systemd"),
            (Self::Alpine, "polkit") => Some("polkit"),
            (Self::Alpine, "nmcli") => Some("networkmanager-cli"),
            (Self::Alpine, "smbclient") => Some("samba-client"),
            (Self::Alpine, "mount.cifs") => Some("cifs-utils"),
            (Self::Alpine, "mount.nfs") => Some("nfs-utils"),
            (Self::Alpine, "avahi-browse") => Some("avahi-tools"),
            (Self::Alpine, "smbd") => Some("samba"),
            (Self::Alpine, "testparm") => Some("samba-common-tools"),
            (Self::Alpine, "smbpasswd") => Some("samba-common-tools"),

            (Self::Arch, "aplay") => Some("alsa-utils"),
            (Self::Arch, "amixer") => Some("alsa-utils"),
            (Self::Arch, "mpc") => Some("mpc"),
            (Self::Arch, "mpd") => Some("mpd"),
            (Self::Arch, "curl") => Some("curl"),
            (Self::Arch, "ffmpeg") => Some("ffmpeg"),
            (Self::Arch, "systemctl") => Some("systemd"),
            (Self::Arch, "polkit") => Some("polkit"),
            (Self::Arch, "nmcli") => Some("networkmanager"),
            (Self::Arch, "smbclient") => Some("smbclient"),
            (Self::Arch, "mount.cifs") => Some("cifs-utils"),
            (Self::Arch, "mount.nfs") => Some("nfs-utils"),
            (Self::Arch, "avahi-browse") => Some("avahi"),
            (Self::Arch, "smbd") => Some("samba"),
            (Self::Arch, "testparm") => Some("samba"),
            (Self::Arch, "smbpasswd") => Some("samba"),

            _ => None,
        }
    }
}

/// Parse the `ID=` field from `/etc/os-release` text.
///
/// Public for unit testability — [`DistroFamily::detect_local`]
/// wraps this with the filesystem read.
pub fn parse_os_release_id(text: &str) -> DistroFamily {
    let mut id = None;
    let mut id_like = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ID=") {
            id = Some(strip_quotes(rest).to_lowercase());
        } else if let Some(rest) = line.strip_prefix("ID_LIKE=") {
            id_like = Some(strip_quotes(rest).to_lowercase());
        }
    }
    let candidates: Vec<String> = match (id, id_like) {
        (Some(a), Some(b)) => {
            let mut out = vec![a];
            out.extend(b.split_whitespace().map(String::from));
            out
        }
        (Some(a), None) => vec![a],
        (None, Some(b)) => b.split_whitespace().map(String::from).collect(),
        (None, None) => vec![],
    };
    for c in &candidates {
        match c.as_str() {
            "debian" | "ubuntu" | "raspbian" | "linuxmint" => {
                return DistroFamily::Debian
            }
            "fedora" | "rhel" | "centos" | "almalinux" | "rocky" => {
                return DistroFamily::Fedora
            }
            "alpine" => return DistroFamily::Alpine,
            "arch" | "manjaro" | "endeavouros" => return DistroFamily::Arch,
            _ => {}
        }
    }
    DistroFamily::Unknown
}

fn strip_quotes(s: &str) -> String {
    let trimmed = s.trim();
    trimmed
        .trim_start_matches('"')
        .trim_end_matches('"')
        .trim_start_matches('\'')
        .trim_end_matches('\'')
        .to_string()
}

/// One structured remediation the operator can apply to close a
/// parity-check failure. Every variant renders to a concrete
/// operator-executable command; none applies automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemediationHint {
    /// Install a package that provides a missing binary.
    /// Rendered as e.g. `apt install alsa-utils`.
    PackageInstall {
        /// Package name resolved for the distribution family
        /// below (e.g. `alsa-utils`, `NetworkManager`).
        package: String,
        /// Distribution family the package name is scoped to.
        distro: DistroFamily,
        /// The binary that motivated the install (e.g. `aplay`).
        /// Included so the operator UI can group hints by cause.
        binary: String,
    },
    /// Load a kernel module that is available on the host but
    /// not currently loaded. Rendered as `modprobe <module>`.
    /// Distinct from a truly-absent module (surface that as
    /// `Custom` with a kernel-config note).
    KernelModule {
        /// Module name as `modprobe` and `/proc/modules` report
        /// it.
        module: String,
    },
    /// Add a supplementary group membership to the plugin's
    /// service identity. Rendered as
    /// `usermod -aG <group> <user>`. Requires operator to
    /// approve the group + user combination.
    SupplementaryGroup {
        /// Group to add (e.g. `audio`, `plugdev`, `video`).
        group: String,
        /// Service identity the group is added to. Empty when
        /// the framework does not know the plugin's declared
        /// service user; the operator UI substitutes the
        /// distribution's default plugin service user.
        user: String,
    },
    /// Install a polkit rules drop-in that authorises the
    /// plugin's required actions. Rendered as a file-copy to
    /// `/etc/polkit-1/rules.d/`.
    PolkitDropIn {
        /// Filename convention: `50-evo-<plugin-id>.rules`.
        policy_file: String,
        /// polkit action identifiers the drop-in authorises
        /// (e.g. `org.freedesktop.NetworkManager.*`). Wildcards
        /// permitted at the trailing component per policy.
        required_actions: Vec<String>,
    },
    /// Enable and start a systemd unit the plugin requires.
    /// Rendered as `systemctl enable --now <unit>`. Distinct
    /// from a truly-absent unit (surface that as
    /// `PackageInstall` for the package that provides it).
    SystemService {
        /// systemd unit name (e.g. `mpd.service`,
        /// `NetworkManager.service`).
        unit: String,
    },
    /// Escape hatch for remediations that do not fit the
    /// structured variants above. The operator UI renders
    /// `description` and offers `command` as an "Apply" action.
    Custom {
        /// Operator-readable description of the failure this
        /// remediation addresses.
        description: String,
        /// Shell command the operator can execute to apply the
        /// remediation. MUST be a single line; the operator UI
        /// renders it verbatim.
        command: String,
    },
}

impl RemediationHint {
    /// Render the hint as an operator-executable shell command.
    /// Every variant produces a single-line command that runs
    /// as-is against a shell (typically wrapped in `sudo` by
    /// the distribution-provided unit-installer).
    pub fn shell_command(&self) -> String {
        match self {
            Self::PackageInstall {
                package, distro, ..
            } => {
                let install = distro.install_command();
                if install.is_empty() {
                    format!(
                        "# install {package} via the local package manager (distro not detected)"
                    )
                } else {
                    format!("{install} {package}")
                }
            }
            Self::KernelModule { module } => format!("modprobe {module}"),
            Self::SupplementaryGroup { group, user } => {
                if user.is_empty() {
                    format!(
                        "usermod -aG {group} <the plugin's service identity>"
                    )
                } else {
                    format!("usermod -aG {group} {user}")
                }
            }
            Self::PolkitDropIn { policy_file, .. } => format!(
                "install /etc/polkit-1/rules.d/{policy_file} (bundle-provided)"
            ),
            Self::SystemService { unit } => {
                format!("systemctl enable --now {unit}")
            }
            Self::Custom { command, .. } => command.clone(),
        }
    }

    /// Operator-readable one-line summary. Used by the CLI's
    /// check-report output.
    pub fn summary(&self) -> String {
        match self {
            Self::PackageInstall {
                package,
                distro,
                binary,
            } => format!(
                "install `{package}` ({}) to provide `{binary}`",
                distro.as_str()
            ),
            Self::KernelModule { module } => {
                format!("load kernel module `{module}`")
            }
            Self::SupplementaryGroup { group, user } => {
                if user.is_empty() {
                    format!(
                        "add supplementary group `{group}` to the plugin's service identity"
                    )
                } else {
                    format!("add `{user}` to group `{group}`")
                }
            }
            Self::PolkitDropIn {
                policy_file,
                required_actions,
            } => format!(
                "install polkit drop-in `{policy_file}` authorising: {}",
                required_actions.join(", ")
            ),
            Self::SystemService { unit } => {
                format!("enable and start systemd unit `{unit}`")
            }
            Self::Custom { description, .. } => description.clone(),
        }
    }
}

/// List of remediation hints derived from a set of parity-check
/// failures. Empty when every parity item passed. Ordered stably
/// so the operator UI renders the same list on every run.
pub type RemediationList = Vec<RemediationHint>;

/// Derive a [`RemediationHint`] for a missing binary. Uses the
/// distro family's package registry; falls back to `Custom`
/// naming the binary when the registry has no entry.
pub fn hint_for_missing_binary(
    binary: &str,
    distro: DistroFamily,
) -> RemediationHint {
    match distro.binary_to_package(binary) {
        Some(package) => RemediationHint::PackageInstall {
            package: package.to_string(),
            distro,
            binary: binary.to_string(),
        },
        None => {
            // No curated entry — fall back to the binary name
            // (many packages ship a same-named binary; the
            // operator can confirm before apply). Emit as
            // Custom so the UI does not present it as a
            // curated recommendation.
            let install = distro.install_command();
            let cmd = if install.is_empty() {
                format!("# install a package providing `{binary}` via the local package manager")
            } else {
                format!("{install} {binary}   # verify package name before applying")
            };
            RemediationHint::Custom {
                description: format!(
                    "install a package providing `{binary}` (no curated entry for {})",
                    distro.as_str()
                ),
                command: cmd,
            }
        }
    }
}

/// Derive a hint for a missing kernel module.
pub fn hint_for_missing_module(module: &str) -> RemediationHint {
    RemediationHint::KernelModule {
        module: module.to_string(),
    }
}

/// Derive a hint for a missing supplementary group membership.
/// `user` may be empty when the framework does not know the
/// plugin's declared service identity yet; the summary + shell
/// command make that explicit.
pub fn hint_for_missing_group(group: &str, user: &str) -> RemediationHint {
    RemediationHint::SupplementaryGroup {
        group: group.to_string(),
        user: user.to_string(),
    }
}

/// Derive a hint for a missing systemd unit.
pub fn hint_for_missing_service(unit: &str) -> RemediationHint {
    RemediationHint::SystemService {
        unit: unit.to_string(),
    }
}

/// Derive a hint for an absent polkit drop-in.
pub fn hint_for_polkit(
    policy_file: &str,
    required_actions: &[String],
) -> RemediationHint {
    RemediationHint::PolkitDropIn {
        policy_file: policy_file.to_string(),
        required_actions: required_actions.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_os_release_id_recognises_debian_ids() {
        assert_eq!(parse_os_release_id("ID=debian\n"), DistroFamily::Debian);
        assert_eq!(parse_os_release_id("ID=ubuntu\n"), DistroFamily::Debian);
        assert_eq!(parse_os_release_id("ID=raspbian\n"), DistroFamily::Debian);
    }

    #[test]
    fn parse_os_release_id_recognises_fedora_family() {
        assert_eq!(parse_os_release_id("ID=fedora\n"), DistroFamily::Fedora);
        assert_eq!(parse_os_release_id("ID=rhel\n"), DistroFamily::Fedora);
        assert_eq!(parse_os_release_id("ID=rocky\n"), DistroFamily::Fedora);
    }

    #[test]
    fn parse_os_release_id_recognises_alpine_arch() {
        assert_eq!(parse_os_release_id("ID=alpine\n"), DistroFamily::Alpine);
        assert_eq!(parse_os_release_id("ID=arch\n"), DistroFamily::Arch);
        assert_eq!(parse_os_release_id("ID=manjaro\n"), DistroFamily::Arch);
    }

    #[test]
    fn parse_os_release_id_handles_quoted_ids() {
        assert_eq!(
            parse_os_release_id("ID=\"debian\"\n"),
            DistroFamily::Debian
        );
        assert_eq!(parse_os_release_id("ID='fedora'\n"), DistroFamily::Fedora);
    }

    #[test]
    fn parse_os_release_id_falls_back_to_id_like() {
        // Downstream derivatives typically set ID to the
        // derivative name and ID_LIKE to the upstream family.
        let text = "ID=pop\nID_LIKE=\"ubuntu debian\"\n";
        assert_eq!(parse_os_release_id(text), DistroFamily::Debian);
    }

    #[test]
    fn parse_os_release_id_returns_unknown_for_absent_or_alien() {
        assert_eq!(parse_os_release_id(""), DistroFamily::Unknown);
        assert_eq!(parse_os_release_id("ID=haiku\n"), DistroFamily::Unknown);
    }

    #[test]
    fn binary_to_package_covers_audio_domain_binaries() {
        assert_eq!(
            DistroFamily::Debian.binary_to_package("aplay"),
            Some("alsa-utils")
        );
        assert_eq!(
            DistroFamily::Fedora.binary_to_package("nmcli"),
            Some("NetworkManager")
        );
        assert_eq!(
            DistroFamily::Alpine.binary_to_package("nmcli"),
            Some("networkmanager-cli")
        );
        assert_eq!(
            DistroFamily::Arch.binary_to_package("mount.cifs"),
            Some("cifs-utils")
        );
    }

    #[test]
    fn binary_to_package_returns_none_for_uncurated_binary() {
        assert_eq!(
            DistroFamily::Debian.binary_to_package("some-obscure-binary"),
            None
        );
    }

    #[test]
    fn hint_for_missing_binary_uses_curated_entry_when_present() {
        let h = hint_for_missing_binary("aplay", DistroFamily::Debian);
        assert!(matches!(
            &h,
            RemediationHint::PackageInstall { package, distro, binary }
            if package == "alsa-utils"
                && matches!(distro, DistroFamily::Debian)
                && binary == "aplay"
        ));
        assert_eq!(h.shell_command(), "apt install alsa-utils");
    }

    #[test]
    fn hint_for_missing_binary_falls_back_to_custom_when_uncurated() {
        let h = hint_for_missing_binary("obscure-tool", DistroFamily::Debian);
        assert!(matches!(&h, RemediationHint::Custom { .. }));
        assert!(h.shell_command().contains("apt install obscure-tool"));
    }

    #[test]
    fn hint_for_missing_binary_on_unknown_distro_uses_comment() {
        let h = hint_for_missing_binary("aplay", DistroFamily::Unknown);
        assert!(matches!(&h, RemediationHint::Custom { .. }));
        // Unknown distro has no install command; shell_command
        // renders as a comment for the operator to fill in.
        assert!(h.shell_command().starts_with("# install"));
    }

    #[test]
    fn hint_for_missing_module_renders_modprobe() {
        let h = hint_for_missing_module("snd_aloop");
        assert_eq!(h.shell_command(), "modprobe snd_aloop");
        assert_eq!(h.summary(), "load kernel module `snd_aloop`");
    }

    #[test]
    fn hint_for_missing_group_renders_usermod() {
        let h = hint_for_missing_group("audio", "evo-plugin-user");
        assert_eq!(h.shell_command(), "usermod -aG audio evo-plugin-user");
        assert_eq!(h.summary(), "add `evo-plugin-user` to group `audio`");
    }

    #[test]
    fn hint_for_missing_group_handles_empty_user() {
        let h = hint_for_missing_group("audio", "");
        assert!(h
            .shell_command()
            .contains("<the plugin's service identity>"));
        assert!(h.summary().contains("add supplementary group `audio`"));
    }

    #[test]
    fn hint_for_missing_service_renders_systemctl() {
        let h = hint_for_missing_service("mpd.service");
        assert_eq!(h.shell_command(), "systemctl enable --now mpd.service");
        assert_eq!(h.summary(), "enable and start systemd unit `mpd.service`");
    }

    #[test]
    fn hint_for_polkit_carries_required_actions() {
        let h = hint_for_polkit(
            "50-evo-org.evoframework.network.rules",
            &[
                "org.freedesktop.NetworkManager.enable-disable-network"
                    .to_string(),
                "org.freedesktop.NetworkManager.settings.modify.system"
                    .to_string(),
            ],
        );
        assert!(matches!(
            &h,
            RemediationHint::PolkitDropIn { policy_file, required_actions }
            if policy_file == "50-evo-org.evoframework.network.rules"
                && required_actions.len() == 2
        ));
        assert!(h
            .shell_command()
            .contains("50-evo-org.evoframework.network.rules"));
    }

    #[test]
    fn hint_serialisation_round_trips_through_json() {
        let original = hint_for_missing_binary("aplay", DistroFamily::Debian);
        let json = serde_json::to_string(&original).unwrap();
        let parsed: RemediationHint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn every_variant_serialises_with_stable_kind_tag() {
        let cases: Vec<(RemediationHint, &str)> = vec![
            (
                hint_for_missing_binary("aplay", DistroFamily::Debian),
                "package_install",
            ),
            (hint_for_missing_module("snd_aloop"), "kernel_module"),
            (hint_for_missing_group("audio", "u"), "supplementary_group"),
            (
                hint_for_polkit("50-evo-x.rules", &["a".to_string()]),
                "polkit_drop_in",
            ),
            (hint_for_missing_service("mpd.service"), "system_service"),
            (
                RemediationHint::Custom {
                    description: "x".into(),
                    command: "y".into(),
                },
                "custom",
            ),
        ];
        for (hint, expected_kind) in cases {
            let json: serde_json::Value = serde_json::to_value(&hint).unwrap();
            assert_eq!(
                json.get("kind").and_then(|v| v.as_str()),
                Some(expected_kind),
                "unexpected kind tag for {hint:?}"
            );
        }
    }
}
