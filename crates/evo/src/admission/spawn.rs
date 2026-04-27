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
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::process::Child;

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
pub(crate) async fn wait_or_kill_child(name: &str, child: &mut Child) {
    match tokio::time::timeout(CHILD_SHUTDOWN_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            if status.success() {
                tracing::info!(
                    plugin = %name,
                    "plugin child exited cleanly"
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
