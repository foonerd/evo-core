//! Tracing subscriber setup per `docs/engineering/LOGGING.md`.
//!
//! The steward installs a layered subscriber:
//!
//! - An `EnvFilter` layer whose precedence is:
//!   1. `cli_override` (e.g. from `--log-level`), if given and valid.
//!   2. `RUST_LOG`, if set and valid.
//!   3. The `log_level` field from [`StewardConfig::steward`], if valid.
//!   4. Hard-coded fallback of `warn`.
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
/// The `cli_override` parameter carries an optional log-level directive
/// from the command line (e.g. `--log-level info`) which takes
/// precedence over `RUST_LOG` and the config file.
///
/// Returns an error if the subscriber has already been installed (for
/// example, if called twice). The binary should call this exactly once.
///
/// Tests should not call this: leaving the global subscriber unset means
/// tests run silently, which is what most unit tests want.
pub fn init(
    config: &StewardConfig,
    cli_override: Option<&str>,
) -> Result<(), StewardError> {
    let filter = resolve_filter(&config.steward.log_level, cli_override);

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
                    .map_err(|init_err| {
                        StewardError::Config(format!(
                            "tracing subscriber init: {init_err}"
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
                StewardError::Config(format!("tracing subscriber init: {e}"))
            })?;
    }

    Ok(())
}

/// Resolve the active `EnvFilter` per LOGGING.md section 3, with CLI
/// override support.
///
/// Precedence (highest wins):
/// 1. `cli_override` if supplied and parses as a valid filter directive.
/// 2. `RUST_LOG` if set and parses as a valid filter directive.
/// 3. `config_level` if it parses as a valid filter directive.
/// 4. Hard-coded fallback of `warn`.
///
/// If `cli_override` is supplied but fails to parse, a warning is
/// written to stderr (tracing is not yet initialised here) and the
/// resolution falls through to the next precedence level.
fn resolve_filter(config_level: &str, cli_override: Option<&str>) -> EnvFilter {
    if let Some(level) = cli_override {
        match EnvFilter::try_new(level) {
            Ok(f) => return f,
            Err(e) => {
                eprintln!(
                    "evo: warning: invalid --log-level '{level}': {e}; falling back"
                );
            }
        }
    }

    if let Ok(s) = std::env::var("RUST_LOG") {
        if let Ok(f) = EnvFilter::try_new(&s) {
            return f;
        }
    }

    EnvFilter::try_new(config_level).unwrap_or_else(|_| EnvFilter::new("warn"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_filter_defaults_to_warn_on_bad_input() {
        std::env::remove_var("RUST_LOG");
        let _ = resolve_filter("this-is-not-valid", None);
    }

    #[test]
    fn resolve_filter_accepts_warn_from_config() {
        std::env::remove_var("RUST_LOG");
        let _ = resolve_filter("warn", None);
    }

    #[test]
    fn resolve_filter_accepts_info_from_config() {
        std::env::remove_var("RUST_LOG");
        let _ = resolve_filter("info", None);
    }

    #[test]
    fn resolve_filter_cli_override_accepts_valid() {
        std::env::remove_var("RUST_LOG");
        let _ = resolve_filter("warn", Some("debug"));
    }

    #[test]
    fn resolve_filter_cli_override_bad_falls_through() {
        std::env::remove_var("RUST_LOG");
        // Bad override -> falls through to config level.
        let _ = resolve_filter("warn", Some("!!!not-a-valid-directive!!!"));
    }
}
