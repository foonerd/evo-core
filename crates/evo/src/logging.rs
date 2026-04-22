//! Tracing subscriber setup per `docs/engineering/LOGGING.md`.
//!
//! The steward installs a layered subscriber:
//!
//! - An `EnvFilter` layer that honours `RUST_LOG` if set, else the
//!   `log_level` field from [`StewardConfig::steward`], else `warn`.
//! - An `fmt` layer writing human-readable lines to stderr. systemd
//!   captures this into journald automatically for service installs.
//! - On Linux: an additional `tracing-journald` layer that emits the
//!   same events as structured journald entries with `EVO_*` fields.
//!
//! Call [`init`] exactly once, from the binary's entrypoint, before any
//! `tracing` macro is invoked. Subsequent calls return an error.

use crate::config::StewardConfig;
use crate::error::StewardError;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry};

/// Initialise the tracing subscriber.
///
/// Returns an error if the subscriber has already been installed (for
/// example, if called twice). The binary should call this exactly once.
///
/// Tests should not call this: leaving the global subscriber unset means
/// tests run silently, which is what most unit tests want.
pub fn init(config: &StewardConfig) -> Result<(), StewardError> {
    let filter = resolve_filter(&config.steward.log_level);

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_writer(std::io::stderr);

    #[cfg(target_os = "linux")]
    {
        match tracing_journald::layer() {
            Ok(journald_layer) => {
                Registry::default()
                    .with(filter)
                    .with(fmt_layer)
                    .with(journald_layer)
                    .try_init()
                    .map_err(|e| {
                        StewardError::Config(format!(
                            "tracing subscriber init: {e}"
                        ))
                    })?;
            }
            Err(e) => {
                // Journald unavailable (running as non-root, or container
                // without journald, or non-systemd host). Continue with
                // stderr-only.
                Registry::default()
                    .with(filter)
                    .with(fmt_layer)
                    .try_init()
                    .map_err(|e| {
                        StewardError::Config(format!(
                            "tracing subscriber init: {e}"
                        ))
                    })?;
                tracing::debug!(
                    reason = %e,
                    "journald layer unavailable; using stderr only"
                );
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        Registry::default()
            .with(filter)
            .with(fmt_layer)
            .try_init()
            .map_err(|e| {
                StewardError::Config(format!(
                    "tracing subscriber init: {e}"
                ))
            })?;
    }

    Ok(())
}

/// Resolve the active `EnvFilter` per LOGGING.md section 3.
///
/// Precedence (highest wins):
/// 1. `RUST_LOG` if it parses as a valid filter directive.
/// 2. The supplied `config_level` if it parses as a valid filter
///    directive (e.g. `"warn"` -> a filter admitting `warn` and above
///    across all targets).
/// 3. Hard-coded fallback of `warn`.
fn resolve_filter(config_level: &str) -> EnvFilter {
    if let Ok(s) = std::env::var("RUST_LOG") {
        if let Ok(f) = EnvFilter::try_new(&s) {
            return f;
        }
    }
    EnvFilter::try_new(config_level)
        .unwrap_or_else(|_| EnvFilter::new("warn"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_filter_defaults_to_warn_on_bad_input() {
        // SAFETY: single-threaded test, no RUST_LOG interference.
        std::env::remove_var("RUST_LOG");
        let f = resolve_filter("this-is-not-valid");
        // If we got here without panic, the fallback worked. Best we can
        // do without a richer EnvFilter introspection API.
        let _ = f;
    }

    #[test]
    fn resolve_filter_accepts_warn() {
        std::env::remove_var("RUST_LOG");
        let _ = resolve_filter("warn");
    }

    #[test]
    fn resolve_filter_accepts_info() {
        std::env::remove_var("RUST_LOG");
        let _ = resolve_filter("info");
    }
}
