// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Graceful steward restart primitive.
//!
//! Sub-primitive J of the three-channel update model.
//! Replaces the running steward's process image with a
//! supplied target binary via Unix `execve`, after a brief
//! drain delay during which connected wire clients receive
//! a [`crate::happenings::Happening::StewardRestarting`]
//! notification and child plugin processes are signalled.
//!
//! The new steward boot path rehydrates from durable
//! substrate (every primitive's persistence layer survives
//! exec), reconnects out-of-process plugins (admission re-
//! runs from the persisted plugin set), and accepts wire-
//! client reconnects.
//!
//! ## Current substrate scope
//!
//! - Restart wire op + happening + state machine that
//!   sequences through the drain → child-signal → exec
//!   handoff.
//! - Operator passes the target binary path explicitly
//!   (defaults to `std::env::current_exe()` when omitted —
//!   useful for "restart in place" after a config change).
//! - The new steward inherits the running steward's argv,
//!   so command-line flags carry across.
//!
//! Out of scope, named explicitly:
//!
//! - **Atomic binary swap** (download new binary →
//!   signature verification → atomic rename to the active
//!   path) rides the core update source (sub-primitive
//!   I2); this primitive accepts an already-staged target
//!   path.
//! - **A/B partitioning** (Mender / RAUC / OSTree) is
//!   vendor-distribution scope; the framework's substrate
//!   doesn't preclude it but doesn't mandate it.
//! - **Perfect in-flight wire-request quiescence** rides
//!   bullet-proof integration; the current substrate ships
//!   graceful-with-brief-drain.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use crate::happenings::{Happening, HappeningBus};

/// Configuration for the [`RestartCoordinator`].
#[derive(Debug, Clone)]
pub struct RestartConfig {
    /// Drain delay between emitting the
    /// `StewardRestarting` happening and `execve` of the
    /// new binary. Gives connected wire clients a window
    /// to flush their last subscription frame and prepare
    /// to reconnect. Default 500ms.
    pub drain_delay: Duration,
    /// Whether to send `SIGTERM` to known child plugin
    /// processes before exec. Default `true` — child
    /// processes exit cleanly, the new steward re-spawns
    /// them per the persisted plugin set on its boot.
    pub signal_children: bool,
    /// Maximum time to wait for child processes to exit
    /// after SIGTERM. After this window the coordinator
    /// proceeds to exec regardless. Default 2 seconds.
    pub child_reap_timeout: Duration,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            drain_delay: Duration::from_millis(500),
            signal_children: true,
            child_reap_timeout: Duration::from_secs(2),
        }
    }
}

/// Errors raised by [`RestartCoordinator`].
#[derive(Debug, thiserror::Error)]
pub enum RestartError {
    /// Coordinator could not resolve a target binary path —
    /// no operator-supplied path AND `std::env::current_exe()`
    /// returned an error.
    #[error("could not resolve target binary path: {0}")]
    CouldNotResolveBinary(String),
    /// The supplied target binary path does not exist on
    /// disk.
    #[error("target binary does not exist: {0}")]
    BinaryMissing(PathBuf),
    /// The supplied target binary path exists but is not
    /// executable by the current process.
    #[error("target binary not executable: {0}")]
    BinaryNotExecutable(PathBuf),
    /// `execve` returned an error (`exec` only returns when
    /// the call fails — successful exec replaces the
    /// process image).
    #[error("execve failed: {0}")]
    ExecFailed(std::io::Error),
    /// The coordinator already has a restart in flight;
    /// concurrent restart requests are refused so the
    /// drain → exec sequence runs once.
    #[error("restart already in flight")]
    AlreadyInFlight,
}

/// Operator-issued restart request shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartRequest {
    /// Operator-supplied free-form reason recorded in the
    /// audit trail and surfaced in the
    /// `StewardRestarting` happening.
    pub reason: String,
    /// Optional target binary path. When `None`, the
    /// coordinator uses `std::env::current_exe()` — useful
    /// for "restart in place" applies (config change,
    /// audit log rollover). When `Some`, the operator
    /// supplies an alternative path (the typical use case
    /// when the core update source has staged a new binary).
    #[serde(default)]
    pub target_binary: Option<PathBuf>,
    /// Operator principal recorded in the audit trail.
    /// `None` falls back to the wire client's UID at the
    /// handler.
    #[serde(default)]
    pub approved_by: Option<String>,
}

/// Records the most recent restart request the coordinator
/// received. Surfaced via [`RestartCoordinator::last_request`]
/// to the operator surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartReceipt {
    /// The request the coordinator processed.
    pub request: RestartRequest,
    /// Resolved binary path (after `current_exe()` fallback).
    pub resolved_binary: PathBuf,
    /// Resolved argv (the running steward's argv at the
    /// moment of resolution).
    pub argv: Vec<String>,
    /// Wall-clock millisecond timestamp the request was
    /// recorded.
    pub recorded_at_ms: u64,
}

/// Graceful steward restart coordinator. Holds the current
/// argv (captured at boot) + restart config + an in-flight
/// guard. Cheap to share via `Arc`.
pub struct RestartCoordinator {
    happenings: Arc<HappeningBus>,
    config: RestartConfig,
    argv: Vec<String>,
    inner: AsyncMutex<Inner>,
}

impl std::fmt::Debug for RestartCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestartCoordinator")
            .field("config", &self.config)
            .field("argv", &self.argv)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Inner {
    in_flight: bool,
    last_receipt: Option<RestartReceipt>,
}

impl RestartCoordinator {
    /// Construct a coordinator. The `argv` should be the
    /// process's argv at boot (`std::env::args().collect()`
    /// is the canonical caller). The coordinator inherits
    /// it across restart so command-line flags carry
    /// through.
    pub fn new(
        happenings: Arc<HappeningBus>,
        config: RestartConfig,
        argv: Vec<String>,
    ) -> Self {
        Self {
            happenings,
            config,
            argv,
            inner: AsyncMutex::new(Inner::default()),
        }
    }

    /// Read the most recent restart receipt — useful for
    /// the operator surface when audit narrows on "what
    /// did the operator request, when".
    pub async fn last_request(&self) -> Option<RestartReceipt> {
        let g = self.inner.lock().await;
        g.last_receipt.clone()
    }

    /// Validate a restart request without initiating it.
    /// Returns the resolved binary path on success. Used
    /// by the wire-op handler to refuse before emitting
    /// the happening + draining.
    pub fn validate(
        &self,
        request: &RestartRequest,
    ) -> Result<PathBuf, RestartError> {
        let path = match request.target_binary.as_ref() {
            Some(p) => p.clone(),
            None => std::env::current_exe().map_err(|e| {
                RestartError::CouldNotResolveBinary(e.to_string())
            })?,
        };
        if !path.exists() {
            return Err(RestartError::BinaryMissing(path));
        }
        if !is_executable(&path) {
            return Err(RestartError::BinaryNotExecutable(path));
        }
        Ok(path)
    }

    /// Initiate the graceful restart sequence. On success,
    /// this method does **not** return — `execve` replaces
    /// the process image. On failure, returns an
    /// [`RestartError`] without disturbing the running
    /// steward. The coordinator refuses concurrent
    /// invocations: the second caller receives
    /// [`RestartError::AlreadyInFlight`].
    ///
    /// Sequence:
    ///
    /// 1. Validate the request (resolves binary path).
    /// 2. Mark coordinator in-flight.
    /// 3. Emit
    ///    [`crate::happenings::Happening::StewardRestarting`].
    /// 4. Sleep [`RestartConfig::drain_delay`] so wire
    ///    clients see the happening + flush.
    /// 5. (Future hook) signal child processes via
    ///    `SIGTERM`; today the signal-children flag is
    ///    declared but the child registry from which we'd
    ///    enumerate PIDs is wired by the admission engine —
    ///    the framework substrate ships the flag + sleep;
    ///    integrating against the engine's child-process
    ///    map rides a follow-on iteration.
    /// 6. `execve` the target binary with the running
    ///    steward's argv. On success, this never returns;
    ///    the new process image starts running.
    pub async fn initiate(
        self: &Arc<Self>,
        request: RestartRequest,
    ) -> Result<std::convert::Infallible, RestartError> {
        let resolved = self.validate(&request)?;
        {
            let mut g = self.inner.lock().await;
            if g.in_flight {
                return Err(RestartError::AlreadyInFlight);
            }
            g.in_flight = true;
            g.last_receipt = Some(RestartReceipt {
                request: request.clone(),
                resolved_binary: resolved.clone(),
                argv: self.argv.clone(),
                recorded_at_ms: now_ms(),
            });
        }

        // Emit the happening so connected wire clients know
        // the steward is restarting. Drain delay below
        // gives them a beat to read the frame + close
        // cleanly.
        self.happenings
            .emit_durable(Happening::StewardRestarting {
                reason: request.reason.clone(),
                expected_downtime_ms: self.config.drain_delay.as_millis()
                    as u64
                    + self.config.child_reap_timeout.as_millis() as u64,
                target_binary: resolved.display().to_string(),
                approved_by: request
                    .approved_by
                    .clone()
                    .unwrap_or_else(|| "operator".to_string()),
                at: std::time::SystemTime::now(),
            })
            .await
            .map_err(|e| {
                RestartError::ExecFailed(std::io::Error::other(format!(
                    "emit StewardRestarting happening: {e}"
                )))
            })?;

        tokio::time::sleep(self.config.drain_delay).await;

        if self.config.signal_children {
            // Child-process discipline: out-of-process
            // plugin children are reaped by the admission
            // engine's drain on exit. The framework
            // substrate ships the flag; integrating against
            // the engine's PID map for explicit SIGTERM is
            // a follow-on tightening (rides on top of this
            // primitive — the running new steward already
            // re-spawns OOP plugins from the persisted set
            // on its boot).
            tokio::time::sleep(self.config.child_reap_timeout).await;
        }

        // execve. argv[0] is the binary path; subsequent
        // args are the running steward's argv tail.
        let mut cmd = Command::new(&resolved);
        if self.argv.len() > 1 {
            cmd.args(&self.argv[1..]);
        }
        let err = cmd.exec();
        // exec() only returns on failure.
        Err(RestartError::ExecFailed(err))
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let perms = metadata.permissions();
    // Any of the executable bits set is sufficient for
    // current-user execve.
    perms.mode() & 0o111 != 0
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn coordinator() -> Arc<RestartCoordinator> {
        Arc::new(RestartCoordinator::new(
            Arc::new(HappeningBus::with_capacity(64)),
            RestartConfig::default(),
            vec!["evo".to_string(), "--config".to_string(), "x".to_string()],
        ))
    }

    #[test]
    fn restart_config_default_drain_delay_is_500_ms() {
        let c = RestartConfig::default();
        assert_eq!(c.drain_delay, Duration::from_millis(500));
        assert!(c.signal_children);
        assert_eq!(c.child_reap_timeout, Duration::from_secs(2));
    }

    #[test]
    fn validate_refuses_missing_binary() {
        let c = coordinator();
        let r = RestartRequest {
            reason: "test".into(),
            target_binary: Some(PathBuf::from(
                "/no/such/path/to/anything/here",
            )),
            approved_by: None,
        };
        let err = c.validate(&r).unwrap_err();
        assert!(matches!(err, RestartError::BinaryMissing(_)));
    }

    #[test]
    fn validate_refuses_non_executable_binary() {
        let c = coordinator();
        // Create a temp file that exists but is not
        // executable.
        let tmp = std::env::temp_dir()
            .join(format!("evo-restart-test-{}", std::process::id()));
        std::fs::write(&tmp, b"not an executable").unwrap();
        let mut perms = std::fs::metadata(&tmp).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&tmp, perms).unwrap();
        let r = RestartRequest {
            reason: "test".into(),
            target_binary: Some(tmp.clone()),
            approved_by: None,
        };
        let err = c.validate(&r).unwrap_err();
        let _ = std::fs::remove_file(&tmp);
        assert!(matches!(err, RestartError::BinaryNotExecutable(_)));
    }

    #[test]
    fn validate_accepts_executable_binary() {
        let c = coordinator();
        let tmp = std::env::temp_dir()
            .join(format!("evo-restart-test-ok-{}", std::process::id()));
        std::fs::write(&tmp, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&tmp).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms).unwrap();
        let r = RestartRequest {
            reason: "test".into(),
            target_binary: Some(tmp.clone()),
            approved_by: None,
        };
        let resolved = c.validate(&r).unwrap();
        assert_eq!(resolved, tmp);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn validate_falls_back_to_current_exe_when_target_is_none() {
        let c = coordinator();
        let r = RestartRequest {
            reason: "test".into(),
            target_binary: None,
            approved_by: None,
        };
        // current_exe in the test process is the test
        // binary itself, which is executable.
        let resolved = c.validate(&r).unwrap();
        assert_eq!(resolved, std::env::current_exe().unwrap());
    }

    #[tokio::test]
    async fn last_request_is_none_until_initiate_records() {
        let c = coordinator();
        assert!(c.last_request().await.is_none());
    }

    #[test]
    fn restart_request_round_trips_via_serde() {
        let r = RestartRequest {
            reason: "test".into(),
            target_binary: Some(PathBuf::from("/opt/evo/bin/evo")),
            approved_by: Some("alice".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: RestartRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
