//! Out-of-process plugin lifecycle: socket-readiness polling and
//! child-process teardown.
//!
//! When the admission engine spawns a plugin binary, three concerns
//! are common to every out-of-process admission path:
//!
//! 1. Wait for the freshly spawned child to bind and accept on its Unix
//!    socket — bounded by [`SOCKET_READY_TIMEOUT`], polled at
//!    [`SOCKET_POLL_INTERVAL`], aborted early if the child exits before
//!    the socket is ready.
//!
//! 2. During shutdown, unload each admitted plugin and wait its child
//!    process. The unload writes EOF on the wire, the child reacts by
//!    exiting; the steward bounds the wait at
//!    [`CHILD_SHUTDOWN_TIMEOUT`] and SIGKILLs holdouts.
//!
//! 3. During staged shutdown, when a plugin's unload task itself misses
//!    the global deadline, the supervisor takes the child off the
//!    router entry and SIGKILLs it directly.
//!
//! The helpers here implement those three concerns. They are pure
//! functions that take whatever they need (a path, a child handle, a
//! router entry) so neither the engine nor the router needs to know
//! about timeouts or signal mechanics.

use crate::error::StewardError;
use crate::router::{take_child, unload_handle, PluginEntry};
use evo_plugin_sdk::contract::PluginError;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::process::Child;

/// Classify a child-process exit status against the lifecycle
/// contract. Returns `true` when the exit is a "normal-shutdown
/// signal termination" — the expected outcome under systemd
/// `KillMode=control-group` (cgroup-wide SIGTERM hits every
/// plugin alongside the steward) and when the steward's own
/// holdout-kill has fired SIGKILL after the unload deadline.
///
/// Per `docs/engineering/LOGGING.md` §2 these are debug-level
/// events ("the lifecycle event itself"), not warn or error.
/// Other signal terminations (SIGSEGV, SIGABRT, SIGBUS) ARE
/// genuinely anomalous and stay at warn — they indicate plugin
/// crashes the operator may want to notice.
fn is_signal_terminated_normally(status: &ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match status.signal() {
            // SIGTERM (15): graceful termination — systemd
            // `KillMode=control-group` default, manual `kill`.
            // SIGINT (2): operator Ctrl-C in dev runs.
            // SIGKILL (9): steward holdout-kill after unload
            //   deadline elapsed, or operator force-kill.
            // SIGHUP (1): controlling-terminal hangup, treated
            //   as graceful by convention.
            Some(15) | Some(2) | Some(9) | Some(1) => true,
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        false
    }
}

/// Classify an unload-side `PluginError` as "expected because
/// the plugin process is already gone" vs a real failure. The
/// canonical expected case is a Fatal-class wire-disconnected
/// error: the plugin received SIGTERM (cgroup-wide), exited,
/// closed its end of the wire, and the steward's wire-unload
/// reaches a closed socket. Other wire-disconnected sources
/// (plugin crashed mid-run, network drop on a future remote
/// transport) share the same shape and the same lifecycle
/// meaning — the wire-unload is moot because the plugin is
/// gone, not because the unload is broken.
///
/// Returns `true` for the expected case so the caller can
/// downgrade the log level from error to debug per the
/// LOGGING.md contract.
fn is_unload_wire_already_closed(err: &PluginError) -> bool {
    matches!(
        err,
        PluginError::Fatal { context, .. }
            if context.contains("wire unload")
                && context.contains("wire disconnected")
    )
}

/// Timeout for child process exit during shutdown. After this elapses
/// the steward stops waiting politely and kills the child.
pub(crate) const CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for a freshly spawned plugin child to bind and accept on
/// its Unix socket.
pub(crate) const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval when waiting for a plugin socket to be ready.
pub(crate) const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Poll the plugin socket until the child binds it, or the child
/// exits, or the timeout elapses.
///
/// Returns the connected stream on success. On failure, the caller
/// is responsible for killing and reaping the child.
pub(crate) async fn wait_for_socket_ready(
    socket_path: &Path,
    child: &mut Child,
) -> Result<UnixStream, StewardError> {
    let deadline = Instant::now() + SOCKET_READY_TIMEOUT;
    loop {
        // If the child has already exited, stop polling.
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(StewardError::Admission(format!(
                    "plugin child exited before socket was ready: {status:?}"
                )));
            }
            Ok(None) => {} // still running
            Err(e) => {
                return Err(StewardError::Admission(format!(
                    "error polling plugin child: {e}"
                )));
            }
        }

        match UnixStream::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() >= deadline => {
                return Err(StewardError::Admission(format!(
                    "timed out waiting for plugin socket at {} after {:?}",
                    socket_path.display(),
                    SOCKET_READY_TIMEOUT
                )));
            }
            Err(_) => {
                tokio::time::sleep(SOCKET_POLL_INTERVAL).await;
            }
        }
    }
}

/// Wait for a plugin's child process to exit; after a bounded timeout
/// kill it.
///
/// All errors are logged; none are propagated. The child is either
/// reaped cleanly or forcibly killed, in both cases leaving no zombie.
///
/// Log-level discipline (per `docs/engineering/LOGGING.md` §2):
///
/// - **Clean exit (status 0):** info — normal lifecycle narrative.
/// - **Killed by SIGTERM / SIGINT / SIGKILL:** debug — the
///   *expected* outcome under systemd `KillMode=control-group`
///   (cgroup-wide SIGTERM hits every plugin alongside the
///   steward), or when the steward's own holdout-kill fired.
///   These exits are not anomalies the operator needs to
///   notice; they are the lifecycle event itself.
/// - **Killed by another signal (SIGSEGV, SIGABRT, etc.):**
///   warn — recoverable anomaly worth noticing (the plugin
///   crashed; admission may restart it on operator action).
/// - **Non-zero exit code (not signal):** warn — the plugin
///   returned a non-zero exit status of its own accord;
///   investigate.
/// - **Wait syscall failure or kill failure:** error —
///   genuinely broken (the OS or steward couldn't observe /
///   control the child).
pub(crate) async fn wait_or_kill_child(name: &str, child: &mut Child) {
    match tokio::time::timeout(CHILD_SHUTDOWN_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            if status.success() {
                tracing::info!(
                    plugin = %name,
                    "plugin child exited cleanly"
                );
            } else if is_signal_terminated_normally(&status) {
                tracing::debug!(
                    plugin = %name,
                    ?status,
                    "plugin child terminated by expected signal \
                     (cgroup SIGTERM, steward holdout-kill, or \
                     equivalent); not an error"
                );
            } else {
                tracing::warn!(
                    plugin = %name,
                    ?status,
                    "plugin child exited with non-zero status"
                );
            }
        }
        Ok(Err(e)) => {
            tracing::error!(
                plugin = %name,
                error = %e,
                "plugin child wait failed"
            );
        }
        Err(_) => {
            tracing::warn!(
                plugin = %name,
                timeout_ms = CHILD_SHUTDOWN_TIMEOUT.as_millis() as u64,
                "plugin child did not exit after disconnection, killing"
            );
            if let Err(e) = child.kill().await {
                tracing::error!(
                    plugin = %name,
                    error = %e,
                    "plugin child kill failed"
                );
            }
            // Reap the zombie regardless of kill success.
            let _ = child.wait().await;
        }
    }
}

/// Unload a single admitted plugin, handling both in-process and
/// out-of-process cases.
///
/// The entry is the cloned `Arc<PluginEntry>` returned by
/// [`PluginRouter::drain_in_reverse_admission_order`]; its inner
/// handle is taken (not borrowed) so the wire client is dropped
/// before the child is awaited.
///
/// [`PluginRouter::drain_in_reverse_admission_order`]: crate::router::PluginRouter::drain_in_reverse_admission_order
pub(crate) async fn unload_one_plugin(
    entry: Arc<PluginEntry>,
) -> Result<(), StewardError> {
    let name = entry.name.clone();
    let shelf = entry.shelf.clone();

    tracing::info!(
        plugin = %name,
        shelf = %shelf,
        "plugin unloading"
    );

    // Take the handle out of the entry so we can both call
    // `unload()` on it AND drop it before awaiting the child. For
    // in-process plugins this drop is a no-op. For wire-backed
    // plugins the drop closes the WireClient's writer channel,
    // which causes the child to see EOF on its read side. Waiting
    // on the child while still holding the writer would deadlock.
    let unload_result = unload_handle(&entry).await;

    match &unload_result {
        Ok(()) => tracing::info!(plugin = %name, "plugin unloaded"),
        Err(e) if is_unload_wire_already_closed(e) => {
            // Expected lifecycle event: the plugin process is
            // already gone (cgroup SIGTERM, crash, network
            // drop on a future remote transport), so the
            // wire-unload reached a closed socket. Per
            // `docs/engineering/LOGGING.md` §2 this is a
            // debug-level event ("the lifecycle event
            // itself"), not error or warn — there is nothing
            // for the operator to do. The subsequent
            // `wait_or_kill_child` call reaps the already-
            // dead child; the framework's claim sweep
            // (admission drain stage 4) cleans up the
            // plugin's registry claims regardless.
            tracing::debug!(
                plugin = %name,
                error = %e,
                "plugin already exited before wire-unload reached \
                 it; wire-unload was a no-op (this is the \
                 expected outcome under systemd \
                 KillMode=control-group)"
            );
        }
        Err(e) => tracing::error!(
            plugin = %name,
            error = %e,
            "plugin unload failed"
        ),
    }

    if let Some(mut child) = take_child(&entry).await {
        wait_or_kill_child(&name, &mut child).await;
    }

    unload_result.map_err(StewardError::Plugin)
}

/// Staged-shutdown helper: take the child off `entry` and SIGKILL+reap it.
///
/// Idempotent against a task that already completed `take_child`
/// (the slot will be `None`); in-process plugins also pass through
/// here harmlessly.
pub(crate) async fn kill_holdout_child(name: &str, entry: &Arc<PluginEntry>) {
    let mut slot = entry.child.lock().await;
    let Some(mut child) = slot.take() else {
        tracing::warn!(
            plugin = %name,
            "plugin missed shutdown deadline (no child to kill)"
        );
        return;
    };
    tracing::warn!(
        plugin = %name,
        "plugin missed shutdown deadline; sending SIGKILL"
    );
    if let Err(e) = child.kill().await {
        tracing::error!(
            plugin = %name,
            error = %e,
            "plugin SIGKILL failed"
        );
    }
    // Reap so we do not leak a zombie even on kill failure.
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unload_wire_already_closed_classifier_recognises_canonical_shape() {
        // The wire-disconnected case the steward hits during
        // shutdown: PluginError::Fatal { context: "wire unload:
        // wire disconnected", source: ... }.
        let err = PluginError::fatal(
            "wire unload: wire disconnected",
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "wire connection closed",
            ),
        );
        assert!(is_unload_wire_already_closed(&err));
    }

    #[test]
    fn unload_wire_already_closed_classifier_rejects_other_fatal_contexts() {
        // A different fatal context (e.g. plugin returned a
        // genuine fatal error from its own unload) is NOT the
        // expected case and must remain at error log level.
        let err = PluginError::fatal(
            "plugin unload returned fatal",
            std::io::Error::other("internal panic"),
        );
        assert!(!is_unload_wire_already_closed(&err));
    }

    #[test]
    fn unload_wire_already_closed_classifier_rejects_non_fatal() {
        // A Permanent error from the plugin's unload is its
        // own kind of refusal and stays at error.
        let err = PluginError::Permanent("some refusal".into());
        assert!(!is_unload_wire_already_closed(&err));
    }

    #[cfg(unix)]
    #[test]
    fn signal_terminated_normally_recognises_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        // Status 15 = WIFSIGNALED with signal 15 (SIGTERM):
        // the canonical shutdown signal under systemd
        // KillMode=control-group.
        let status = ExitStatus::from_raw(15);
        assert!(is_signal_terminated_normally(&status));
    }

    #[cfg(unix)]
    #[test]
    fn signal_terminated_normally_recognises_sigkill() {
        use std::os::unix::process::ExitStatusExt;
        let status = ExitStatus::from_raw(9);
        assert!(is_signal_terminated_normally(&status));
    }

    #[cfg(unix)]
    #[test]
    fn signal_terminated_normally_rejects_segv() {
        use std::os::unix::process::ExitStatusExt;
        // Status 11 = WIFSIGNALED with signal 11 (SIGSEGV):
        // genuine crash; stays at warn.
        let status = ExitStatus::from_raw(11);
        assert!(!is_signal_terminated_normally(&status));
    }

    #[cfg(unix)]
    #[test]
    fn signal_terminated_normally_rejects_non_zero_exit_code() {
        use std::os::unix::process::ExitStatusExt;
        // Plugin exited via exit(1) — not a signal, just a
        // non-zero exit code. Anomalous; stays at warn.
        let status = ExitStatus::from_raw(1 << 8);
        assert!(!is_signal_terminated_normally(&status));
    }
}
