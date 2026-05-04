//! Graceful shutdown on signal.
//!
//! [`wait_for_signal`] returns a future that resolves on the first
//! received SIGTERM or SIGINT (Unix) or Ctrl-C (everywhere). The steward's
//! main loop awaits this future alongside the server task; whichever
//! completes first drives shutdown.

/// Wait for a termination signal.
///
/// On Unix targets, listens for both SIGTERM (sent by systemd during
/// service stop) and SIGINT (sent by Ctrl-C in a terminal). On non-Unix
/// targets, listens only for Ctrl-C.
///
/// The future resolves as soon as any of the monitored signals fires.
/// An `info`-level event is logged naming which signal was received —
/// receiving a shutdown signal is normal lifecycle narrative per
/// `docs/engineering/LOGGING.md` §2 ("info: Normal high-level
/// lifecycle narrative"), not a recoverable anomaly the operator
/// must notice.
///
/// This future is cancellation-safe: dropping it unregisters the signal
/// handlers cleanly.
pub async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            // Per LOGGING.md §2: warn covers "a recoverable
            // anomaly worth noticing — the system continues but
            // an operator may want to know". Failing to install
            // SIGTERM handling means the steward falls back to
            // ctrl-c only — operationally degraded but still
            // running. That is exactly the warn contract; not
            // error (which the contract reserves for "something
            // broken that will not self-correct").
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to install SIGTERM handler; falling back \
                     to ctrl-c only"
                );
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!(signal = "ctrl-c", "shutdown signal received");
                return;
            }
        };

        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to install SIGINT handler; waiting on \
                     SIGTERM only"
                );
                sigterm.recv().await;
                tracing::info!(signal = "SIGTERM", "shutdown signal received");
                return;
            }
        };

        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!(signal = "SIGTERM", "shutdown signal received");
            }
            _ = sigint.recv() => {
                tracing::info!(signal = "SIGINT", "shutdown signal received");
            }
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            // ctrl-c handler install failure on non-Unix targets:
            // same posture as the Unix SIGTERM-handler-install
            // failure above — degraded but the steward's main
            // loop continues to other shutdown triggers (server
            // task exit, etc.). Warn, not error.
            tracing::warn!(error = %e, "ctrl-c handler failed");
        } else {
            tracing::info!(signal = "ctrl-c", "shutdown signal received");
        }
    }
}
