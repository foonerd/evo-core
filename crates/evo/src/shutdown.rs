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
/// A `warn`-level event is logged naming which signal was received.
///
/// This future is cancellation-safe: dropping it unregisters the signal
/// handlers cleanly.
pub async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler; falling back to ctrl-c only");
                let _ = tokio::signal::ctrl_c().await;
                tracing::warn!(signal = "ctrl-c", "shutdown signal received");
                return;
            }
        };

        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGINT handler; waiting on SIGTERM only");
                sigterm.recv().await;
                tracing::warn!(signal = "SIGTERM", "shutdown signal received");
                return;
            }
        };

        tokio::select! {
            _ = sigterm.recv() => {
                tracing::warn!(signal = "SIGTERM", "shutdown signal received");
            }
            _ = sigint.recv() => {
                tracing::warn!(signal = "SIGINT", "shutdown signal received");
            }
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "ctrl-c handler failed");
        } else {
            tracing::warn!(signal = "ctrl-c", "shutdown signal received");
        }
    }
}
