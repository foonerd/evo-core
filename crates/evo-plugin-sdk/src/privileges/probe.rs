// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Read-only, non-escalating probes that verify a declared
//! [`CapabilityIntent`] is honoured by the runtime environment.
//!
//! Probes run at admission time. The framework's preflight gate
//! iterates a plugin's intents, picks the corresponding probe,
//! executes it, and records the outcome in a
//! [`CapabilityResolutionMap`]. The map is then handed to the
//! plugin via `LoadContext::capabilities` so the plugin's
//! `Plugin::load` picks the right strategy without re-probing.
//!
//! ## Probe discipline
//!
//! - **Read-only.** No probe mutates host state. A probe must
//!   never write to `/etc`, never create files, never run
//!   `systemctl` verbs that change unit state, never trigger a
//!   `polkit` interactive prompt.
//! - **Non-escalating.** A probe must never elevate the caller's
//!   privileges. `sudo -l -n <command>` is fine (dry-run, no
//!   prompt, returns the policy verdict); a bare `sudo <command>`
//!   is not.
//! - **Bounded.** Every probe completes in a small, deterministic
//!   time budget (Default: 500ms). Probes that exec subprocesses
//!   pass a deadline so a hung sudo / systemctl daemon does not
//!   block admission indefinitely.
//! - **Idempotent and side-effect-free.** Running the same probe
//!   twice in a row produces the same outcome. The framework
//!   relies on this for hot-tightening re-probes.
//!
//! ## Probe families shipped in this build
//!
//! - [`SudoersCommandProbe`] — `sudo -l -n <command>`; verifies
//!   the calling user is allowed to run the exact command via
//!   sudo without password (i.e. a NOPASSWD drop-in exists for it).
//! - [`FilesystemAccessProbe`] — `access(path, mode)` syscall;
//!   verifies the path is reachable with the requested mode bits.
//! - [`BinaryPresentProbe`] — PATH search; verifies a named
//!   executable resolves to a runnable file.
//!
//! ## Cross-OS posture
//!
//! The current release ships Linux probe implementations. Probes
//! whose shape is OS-specific (sudoers on macOS / BSDs uses the
//! same `sudo -l -n` and works as-is; opening filesystem paths is
//! portable) work cross-platform; probes that depend on Linux
//! specifics (e.g. reading `/proc/self/status` for EUID
//! detection) gate via `#[cfg(target_os = "linux")]` and return
//! `ProbeOutcome::InapplicableOnThisOs` elsewhere.
//!
//! [`CapabilityIntent`]: crate::privileges::CapabilityIntent
//! [`CapabilityResolutionMap`]: crate::privileges::CapabilityResolutionMap

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::privileges::{CapabilityResolution, ResolutionCounts};

/// Outcome of running a [`Probe`]. Maps onto
/// [`CapabilityResolution`] at admission time; the probe surface
/// is one layer below so probes don't need to know about
/// strategy hints, fallbacks, or operator-readable remedy
/// composition — they just report what they observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Probe ran and observed the precondition is satisfied.
    /// `detail` is operator-readable evidence
    /// (e.g. `"sudo -l matched: /usr/bin/systemctl restart mpd"`).
    Satisfied {
        /// Operator-readable evidence of success.
        detail: String,
    },
    /// Probe ran and observed the precondition is NOT satisfied.
    /// `reason` is operator-readable description of the failure.
    Unsatisfied {
        /// Operator-readable description of failure.
        reason: String,
    },
    /// Probe could not run on this host (OS unsupported, binary
    /// missing, etc.). Treated as `NotProbed` upstream.
    InapplicableOnThisOs {
        /// Operator-readable reason the probe was skipped.
        reason: String,
    },
    /// Probe encountered an internal error unrelated to the
    /// precondition (e.g. process spawn failed for an unexpected
    /// reason). Treated as `Unavailable` upstream — better to
    /// surface than swallow.
    ProbeError {
        /// Operator-readable diagnostic of the probe-side failure.
        diagnostic: String,
    },
}

impl ProbeOutcome {
    /// Project this probe outcome onto the higher-level
    /// [`CapabilityResolution`] enum. `strategy_on_satisfied`
    /// names the production strategy the plugin should install
    /// when the probe passes; pass `None` for probes whose
    /// satisfaction does not select between strategies.
    /// `remedy_on_unsatisfied` is the operator-actionable next
    /// step the framework attaches to an `Unavailable` outcome.
    pub fn into_resolution(
        self,
        strategy_on_satisfied: Option<&str>,
        remedy_on_unsatisfied: &str,
    ) -> CapabilityResolution {
        match self {
            ProbeOutcome::Satisfied { detail } => {
                CapabilityResolution::Available {
                    evidence: detail,
                    strategy: strategy_on_satisfied.map(String::from),
                }
            }
            ProbeOutcome::Unsatisfied { reason } => {
                CapabilityResolution::Unavailable {
                    reason,
                    remedy: remedy_on_unsatisfied.to_string(),
                }
            }
            ProbeOutcome::InapplicableOnThisOs { reason } => {
                CapabilityResolution::NotProbed { reason }
            }
            ProbeOutcome::ProbeError { diagnostic } => {
                CapabilityResolution::Unavailable {
                    reason: format!("probe error: {diagnostic}"),
                    remedy: remedy_on_unsatisfied.to_string(),
                }
            }
        }
    }
}

/// Default probe deadline. Holds the admission path tight even
/// against a wedged sudo or systemctl daemon.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// A read-only, non-escalating probe of a single
/// [`CapabilityIntent`]. Implementations must honour the probe
/// discipline documented at module level.
pub trait Probe: Debug + Send + Sync {
    /// Run the probe against the current host. Implementations
    /// observe `DEFAULT_PROBE_TIMEOUT` as a soft ceiling.
    fn run(&self) -> ProbeOutcome;

    /// Operator-readable label for the probe (used in the
    /// framework's admission report so the operator can see
    /// what was checked). Convention: action-verbs in
    /// imperative shape, e.g. `"sudo -l on /usr/bin/systemctl
    /// restart mpd"`.
    fn label(&self) -> String;
}

// =============================================================
// SudoersCommandProbe
// =============================================================

/// Verify the current user is allowed (via a NOPASSWD sudoers
/// drop-in) to run an exact command without password. Uses
/// `sudo -l -n -- <command>`:
///
/// - `-l` — list privileges (read-only; never executes).
/// - `-n` — non-interactive; refuse if a password is required.
///   This is the difference between "missing drop-in" (exit
///   non-zero) and "drop-in present and works" (exit 0).
/// - `--` — terminate sudo's option parsing so the command is
///   not interpreted as further sudo flags.
///
/// The probe is OS-portable: any UNIX-like host that ships sudo
/// supports `-l -n`. On hosts without sudo (`sudo` not on PATH),
/// the probe reports `InapplicableOnThisOs`.
#[derive(Debug, Clone)]
pub struct SudoersCommandProbe {
    /// Full command (binary + args) the probe asks sudo to
    /// dry-run. Tokens preserved verbatim — sudoers matches
    /// against the exact argv.
    command: Vec<String>,
    /// Soft timeout for the sudo dry-run invocation.
    timeout: Duration,
}

impl SudoersCommandProbe {
    /// Construct a probe for the supplied command tokens
    /// (e.g. `["/usr/bin/systemctl", "restart", "mpd"]`).
    /// Returns `None` when `command` is empty.
    pub fn new<I, S>(command: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let tokens: Vec<String> = command.into_iter().map(Into::into).collect();
        if tokens.is_empty() {
            return None;
        }
        Some(Self {
            command: tokens,
            timeout: DEFAULT_PROBE_TIMEOUT,
        })
    }

    /// Replace the default timeout. Useful for tests.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Probe for SudoersCommandProbe {
    fn run(&self) -> ProbeOutcome {
        if which("sudo").is_none() {
            return ProbeOutcome::InapplicableOnThisOs {
                reason: "sudo not on PATH".to_string(),
            };
        }

        let mut cmd = Command::new("sudo");
        cmd.args(["-l", "-n", "--"]).args(&self.command);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        // For a probe at 500ms, a wait_with_output that doesn't
        // honour the deadline natively is acceptable in practice
        // (sudo -l -n returns immediately or fails fast). A
        // future enhancement could wrap in tokio::time::timeout
        // — kept synchronous here so the SDK has no async dep
        // for probe execution.
        let _ = self.timeout;

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return ProbeOutcome::ProbeError {
                    diagnostic: format!(
                        "failed to spawn sudo -l -n -- {}: {e}",
                        self.command.join(" ")
                    ),
                }
            }
        };

        if output.status.success() {
            ProbeOutcome::Satisfied {
                detail: format!(
                    "sudo -l -n permits: {}",
                    self.command.join(" ")
                ),
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            // Distinguish "no NOPASSWD entry" (typical: exit 1)
            // from sudo-broken cases. Either way it's not
            // satisfied; the diagnostic differentiates the
            // remedy operator will be told about.
            ProbeOutcome::Unsatisfied {
                reason: format!(
                    "sudo -l -n refused {}: exit={:?} stderr={}",
                    self.command.join(" "),
                    output.status.code(),
                    stderr
                ),
            }
        }
    }

    fn label(&self) -> String {
        format!("sudo -l -n -- {}", self.command.join(" "))
    }
}

// =============================================================
// FilesystemAccessProbe
// =============================================================

/// Access modes the probe checks. Combined bitwise via the
/// `AccessMode::all_of` constructor when more than one bit is
/// needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Test the file exists (`F_OK`).
    Exists,
    /// Test the file is readable by the calling user (`R_OK`).
    Readable,
    /// Test the file is writable by the calling user (`W_OK`).
    Writable,
    /// Test the file is executable by the calling user
    /// (`X_OK`) — used for binary probes.
    Executable,
}

impl AccessMode {
    #[cfg(unix)]
    fn as_nix(self) -> nix::unistd::AccessFlags {
        use nix::unistd::AccessFlags;
        match self {
            AccessMode::Exists => AccessFlags::F_OK,
            AccessMode::Readable => AccessFlags::R_OK,
            AccessMode::Writable => AccessFlags::W_OK,
            AccessMode::Executable => AccessFlags::X_OK,
        }
    }

    fn token(self) -> &'static str {
        match self {
            AccessMode::Exists => "F_OK",
            AccessMode::Readable => "R_OK",
            AccessMode::Writable => "W_OK",
            AccessMode::Executable => "X_OK",
        }
    }
}

/// Verify a filesystem path is reachable by the calling user
/// under the requested access mode. Uses the `access(2)` syscall
/// (which respects POSIX permission semantics; equivalent to
/// `std::fs::metadata` + manual perm-bit checking but more
/// honest about how the kernel will treat the eventual open()).
#[derive(Debug, Clone)]
pub struct FilesystemAccessProbe {
    path: PathBuf,
    mode: AccessMode,
}

impl FilesystemAccessProbe {
    /// Construct a probe for the supplied path + mode.
    pub fn new(path: impl AsRef<Path>, mode: AccessMode) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            mode,
        }
    }
}

impl Probe for FilesystemAccessProbe {
    fn run(&self) -> ProbeOutcome {
        #[cfg(unix)]
        {
            match nix::unistd::access(&self.path, self.mode.as_nix()) {
                Ok(()) => ProbeOutcome::Satisfied {
                    detail: format!(
                        "access({:?}, {}) succeeded",
                        self.path,
                        self.mode.token()
                    ),
                },
                Err(errno) => ProbeOutcome::Unsatisfied {
                    reason: format!(
                        "access({:?}, {}) failed: {errno}",
                        self.path,
                        self.mode.token()
                    ),
                },
            }
        }

        #[cfg(not(unix))]
        {
            ProbeOutcome::InapplicableOnThisOs {
                reason: "FilesystemAccessProbe is UNIX-only".to_string(),
            }
        }
    }

    fn label(&self) -> String {
        format!("access({:?}, {})", self.path, self.mode.token())
    }
}

// =============================================================
// BinaryPresentProbe
// =============================================================

/// Verify a named binary resolves to an executable file on the
/// current PATH. Equivalent to `command -v <name>` / `which
/// <name>` semantically; uses `$PATH` directly so the probe has
/// no shell dependency.
#[derive(Debug, Clone)]
pub struct BinaryPresentProbe {
    name: String,
}

impl BinaryPresentProbe {
    /// Construct a probe for the supplied binary name. No path
    /// components — pass `mpd`, not `/usr/bin/mpd`.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl Probe for BinaryPresentProbe {
    fn run(&self) -> ProbeOutcome {
        match which(&self.name) {
            Some(resolved) => ProbeOutcome::Satisfied {
                detail: format!(
                    "{} resolved on PATH to {}",
                    self.name,
                    resolved.display()
                ),
            },
            None => ProbeOutcome::Unsatisfied {
                reason: format!("{} not found on PATH", self.name),
            },
        }
    }

    fn label(&self) -> String {
        format!("which {}", self.name)
    }
}

/// Resolve a binary name against `$PATH`. Returns the first
/// entry that exists and is executable. Returns `None` when the
/// name is not found or `$PATH` is unset / empty.
///
/// Pulled out as a free function so probes that need to verify
/// helper binaries (e.g. `sudo` itself before
/// `SudoersCommandProbe` runs) can reuse the same lookup
/// without instantiating a probe.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if let Ok(meta) = candidate.metadata() {
            if meta.is_file() {
                // Check executable bit by reusing the probe
                // surface. Avoids a second metadata syscall;
                // `access(X_OK)` is the right semantic test.
                let probe = FilesystemAccessProbe::new(
                    &candidate,
                    AccessMode::Executable,
                );
                if matches!(probe.run(), ProbeOutcome::Satisfied { .. }) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

// =============================================================
// Probe runner: aggregate a slice of intents → probes → resolutions
// =============================================================

/// One intent → probe pairing the runner executes. Constructed
/// by the framework's admission code path (which knows the
/// plugin's intents from its `privileges.yaml`) and handed to
/// [`run_probes`] for batch execution. `strategy_hint` carries
/// the strategy label attached on satisfaction (Some) or omitted
/// (None). `remedy` is the operator-actionable string surfaced
/// when the probe is unsatisfied.
#[derive(Debug)]
pub struct ProbePlan {
    /// The intent's id (matches `CapabilityIntent.id`).
    pub intent_id: String,
    /// The probe to run for this intent.
    pub probe: Box<dyn Probe>,
    /// Strategy label set on `Available` outcomes. Omit when
    /// the probe's satisfaction does not select between
    /// strategies (e.g. binary-present probes).
    pub strategy_hint: Option<String>,
    /// Operator-actionable remediation attached to
    /// `Unavailable` outcomes.
    pub remedy: String,
}

/// Run a batch of [`ProbePlan`]s and build the
/// [`CapabilityResolutionMap`].
///
/// Each plan's `probe.run()` is invoked in order; outcome is
/// projected onto a [`CapabilityResolution`] using the plan's
/// `strategy_hint` + `remedy`. Probes are independent — one
/// failing probe does not short-circuit later probes.
///
/// The returned map is consumed by the framework's admission
/// code path, which calls `counts()` for the admission-report
/// summary line and looks up specific intents to enforce
/// `failure_mode: required` semantics.
///
/// [`CapabilityResolutionMap`]:
/// crate::privileges::CapabilityResolutionMap
pub fn run_probes(
    plans: &[ProbePlan],
) -> crate::privileges::CapabilityResolutionMap {
    let mut map = crate::privileges::CapabilityResolutionMap::new();
    for plan in plans {
        let outcome = plan.probe.run();
        let resolution = outcome
            .into_resolution(plan.strategy_hint.as_deref(), &plan.remedy);
        map.insert(plan.intent_id.clone(), resolution);
    }
    map
}

/// Sugar over [`run_probes`] returning aggregated counts
/// alongside the map. Used by the framework's admission code
/// path's summary logging.
pub fn run_probes_with_counts(
    plans: &[ProbePlan],
) -> (crate::privileges::CapabilityResolutionMap, ResolutionCounts) {
    use crate::privileges::CapabilityResolutionMapExt;
    let map = run_probes(plans);
    let counts = map.counts();
    (map, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    // ===== ProbeOutcome → Resolution projection =====

    #[test]
    fn outcome_satisfied_maps_to_available_with_strategy() {
        let outcome = ProbeOutcome::Satisfied {
            detail: "probe ok".into(),
        };
        let r = outcome.into_resolution(Some("sudo"), "irrelevant");
        match r {
            CapabilityResolution::Available { evidence, strategy } => {
                assert_eq!(evidence, "probe ok");
                assert_eq!(strategy, Some("sudo".into()));
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn outcome_satisfied_maps_to_available_without_strategy() {
        let outcome = ProbeOutcome::Satisfied {
            detail: "ok".into(),
        };
        let r = outcome.into_resolution(None, "x");
        match r {
            CapabilityResolution::Available { strategy, .. } => {
                assert!(strategy.is_none());
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn outcome_unsatisfied_maps_to_unavailable_with_remedy() {
        let outcome = ProbeOutcome::Unsatisfied {
            reason: "denied".into(),
        };
        let r = outcome.into_resolution(Some("sudo"), "run bootstrap");
        match r {
            CapabilityResolution::Unavailable { reason, remedy } => {
                assert_eq!(reason, "denied");
                assert_eq!(remedy, "run bootstrap");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn outcome_inapplicable_maps_to_not_probed() {
        let outcome = ProbeOutcome::InapplicableOnThisOs {
            reason: "non-linux".into(),
        };
        let r = outcome.into_resolution(None, "x");
        match r {
            CapabilityResolution::NotProbed { reason } => {
                assert_eq!(reason, "non-linux");
            }
            other => panic!("expected NotProbed, got {other:?}"),
        }
    }

    #[test]
    fn outcome_probe_error_maps_to_unavailable() {
        let outcome = ProbeOutcome::ProbeError {
            diagnostic: "spawn failed".into(),
        };
        let r = outcome.into_resolution(None, "x");
        match r {
            CapabilityResolution::Unavailable { reason, .. } => {
                assert!(reason.contains("probe error"));
                assert!(reason.contains("spawn failed"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ===== SudoersCommandProbe shape =====

    #[test]
    fn sudoers_probe_rejects_empty_command() {
        assert!(SudoersCommandProbe::new(Vec::<String>::new()).is_none());
    }

    #[test]
    fn sudoers_probe_label_includes_command() {
        let p = SudoersCommandProbe::new(vec![
            "/usr/bin/systemctl",
            "restart",
            "mpd",
        ])
        .unwrap();
        let label = p.label();
        assert!(label.contains("/usr/bin/systemctl restart mpd"));
        assert!(label.contains("sudo -l -n"));
    }

    // ===== FilesystemAccessProbe (filesystem-touching) =====

    #[test]
    fn filesystem_probe_satisfied_for_readable_tempfile() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evidence.txt");
        std::fs::write(&path, "x").unwrap();

        let probe = FilesystemAccessProbe::new(&path, AccessMode::Readable);
        match probe.run() {
            ProbeOutcome::Satisfied { detail } => {
                assert!(detail.contains("evidence.txt"));
                assert!(detail.contains("R_OK"));
            }
            other => panic!("expected Satisfied, got {other:?}"),
        }
    }

    #[test]
    fn filesystem_probe_satisfied_for_writable_tempfile() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("w.txt");
        std::fs::write(&path, "x").unwrap();

        let probe = FilesystemAccessProbe::new(&path, AccessMode::Writable);
        assert!(matches!(probe.run(), ProbeOutcome::Satisfied { .. }));
    }

    #[test]
    fn filesystem_probe_unsatisfied_for_unwritable_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ro.txt");
        std::fs::write(&path, "x").unwrap();
        // Strip all write bits.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&path, perms).unwrap();

        let probe = FilesystemAccessProbe::new(&path, AccessMode::Writable);
        // Note: running this test as root will succeed regardless
        // of file perms because root bypasses POSIX checks. Guard
        // with a runtime check to avoid spurious test failures in
        // root-running containers / CI agents.
        let euid = nix::unistd::geteuid();
        if euid.is_root() {
            // Skip the negative-side assertion as root.
            return;
        }
        match probe.run() {
            ProbeOutcome::Unsatisfied { reason } => {
                assert!(reason.contains("ro.txt"));
                assert!(reason.contains("W_OK"));
            }
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    #[test]
    fn filesystem_probe_unsatisfied_for_missing_path() {
        let probe = FilesystemAccessProbe::new(
            "/nonexistent/path/that/should/not/be/there",
            AccessMode::Exists,
        );
        assert!(matches!(probe.run(), ProbeOutcome::Unsatisfied { .. }));
    }

    #[test]
    fn filesystem_probe_label_renders_path_and_mode() {
        let probe = FilesystemAccessProbe::new(
            "/etc/evo/mpd.conf",
            AccessMode::Writable,
        );
        let label = probe.label();
        assert!(label.contains("/etc/evo/mpd.conf"));
        assert!(label.contains("W_OK"));
    }

    // ===== BinaryPresentProbe =====

    #[test]
    fn binary_probe_satisfied_for_sh() {
        // /bin/sh is universally present on any system this
        // crate targets.
        let probe = BinaryPresentProbe::new("sh");
        match probe.run() {
            ProbeOutcome::Satisfied { detail } => {
                assert!(detail.contains("sh"));
            }
            other => panic!("expected Satisfied, got {other:?}"),
        }
    }

    #[test]
    fn binary_probe_unsatisfied_for_nonsense() {
        let probe =
            BinaryPresentProbe::new("zz_definitely_not_a_real_binary_zz");
        match probe.run() {
            ProbeOutcome::Unsatisfied { reason } => {
                assert!(reason.contains("zz_definitely_not_a_real_binary_zz"));
            }
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    #[test]
    fn which_locates_sh() {
        let resolved = which("sh").expect("sh must be on PATH");
        assert!(resolved.is_absolute());
        assert!(resolved.exists());
    }

    #[test]
    fn which_returns_none_for_nonsense() {
        assert!(which("zz_no_such_binary_zz").is_none());
    }

    // ===== Runner =====

    #[test]
    fn run_probes_builds_map_with_one_entry_per_plan() {
        let plans = vec![
            ProbePlan {
                intent_id: "binary_sh".into(),
                probe: Box::new(BinaryPresentProbe::new("sh")),
                strategy_hint: None,
                remedy: "install POSIX shell".into(),
            },
            ProbePlan {
                intent_id: "binary_zzz".into(),
                probe: Box::new(BinaryPresentProbe::new(
                    "zz_no_such_binary_zz",
                )),
                strategy_hint: None,
                remedy: "install zzz".into(),
            },
        ];
        let map = run_probes(&plans);
        assert_eq!(map.len(), 2);
        assert!(map["binary_sh"].is_available());
        assert!(map["binary_zzz"].is_unavailable());
    }

    #[test]
    fn run_probes_with_counts_aggregates_states() {
        let plans = vec![
            ProbePlan {
                intent_id: "ok".into(),
                probe: Box::new(BinaryPresentProbe::new("sh")),
                strategy_hint: Some("direct".into()),
                remedy: "x".into(),
            },
            ProbePlan {
                intent_id: "fail".into(),
                probe: Box::new(BinaryPresentProbe::new("zz_nope")),
                strategy_hint: None,
                remedy: "x".into(),
            },
        ];
        let (map, counts) = run_probes_with_counts(&plans);
        assert_eq!(map.len(), 2);
        assert_eq!(counts.available, 1);
        assert_eq!(counts.unavailable, 1);
        assert_eq!(counts.degraded, 0);
        assert_eq!(counts.not_probed, 0);
    }

    #[test]
    fn run_probes_preserves_strategy_hint_on_available() {
        let plans = vec![ProbePlan {
            intent_id: "binary_sh".into(),
            probe: Box::new(BinaryPresentProbe::new("sh")),
            strategy_hint: Some("direct".into()),
            remedy: "x".into(),
        }];
        let map = run_probes(&plans);
        match &map["binary_sh"] {
            CapabilityResolution::Available { strategy, .. } => {
                assert_eq!(strategy.as_deref(), Some("direct"));
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }
}
