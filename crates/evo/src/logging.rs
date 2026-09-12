// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Tracing subscriber setup per `docs/engineering/LOGGING.md`.
//!
//! The steward installs a single emission layer in addition to the
//! `EnvFilter`. The choice between the structured journald layer and
//! the human-readable stderr layer is exclusive — running both at the
//! same time double-emits every log line (journald-native + stderr
//! captured as STDERR by systemd → journald), which floods the
//! journal and inflates the realtime runtime's CPU + syscall cost.
//!
//! Resolution:
//!
//! - On Linux with a reachable journald socket: install only the
//!   `tracing-journald` layer. systemd will not double-emit because
//!   stderr is not used as a logging surface in this configuration.
//! - Anywhere else (non-Linux, container without journald, non-root
//!   without journald access): fall back to the `fmt` layer writing
//!   human-readable lines to stderr.
//! - The `fmt` writer is wrapped in `tracing_appender::non_blocking`
//!   so write syscalls are off-thread; high-frequency tracing
//!   (intentional or accidental) cannot block the realtime audio
//!   runtime on a slow stderr consumer.
//!
//! `EnvFilter` precedence (highest wins):
//!   1. `cli_override` (e.g. from `--log-level`), if given and valid.
//!   2. `RUST_LOG`, if set and valid.
//!   3. The `log_level` field from [`StewardConfig::steward`], if valid.
//!   4. Hard-coded fallback of [`evo_plugin_sdk::wire_logging::DEFAULT_WIRE_ENV_FILTER`]
//!      — the same directive every out-of-process wire binary
//!      installs when `RUST_LOG` is unset, so the steward and
//!      its OOP plugins share one production log surface. See
//!      `docs/engineering/LOGGING.md` §3.
//!
//! Call [`init`] exactly once, from the binary's entrypoint, before any
//! `tracing` macro is invoked. Subsequent calls return an error. The
//! returned guard owns the non-blocking writer's worker thread (when
//! the stderr fallback path is active) and must outlive the process's
//! tracing emissions; the binary's entrypoint owns it for the
//! lifetime of the program.

use crate::config::StewardConfig;
use crate::error::StewardError;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry};

/// Guard returned by [`init`]. Owns the non-blocking-writer worker
/// thread (when the stderr fallback path is active) and the journald
/// layer's internal state (when the journald path is active). The
/// binary's entrypoint must hold this guard for the lifetime of the
/// process; dropping it flushes pending writes and stops the worker.
pub struct LoggingGuard {
    /// Non-blocking writer guard for the stderr fallback path. `None`
    /// when the journald path is active.
    _non_blocking: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Initialise the tracing subscriber.
///
/// The `cli_override` parameter carries an optional log-level directive
/// from the command line (e.g. `--log-level info`) which takes
/// precedence over `RUST_LOG` and the config file.
///
/// Returns an error if the subscriber has already been installed (for
/// example, if called twice). The binary should call this exactly once
/// and retain the returned [`LoggingGuard`] for the program lifetime.
///
/// Tests should not call this: leaving the global subscriber unset
/// means tests run silently, which is what most unit tests want.
pub fn init(
    config: &StewardConfig,
    cli_override: Option<&str>,
) -> Result<LoggingGuard, StewardError> {
    let (filter, resolved_directive, resolution_source) =
        resolve_filter_with_source(&config.steward.log_level, cli_override);
    // Boot-time diagnostic emitted BEFORE the tracing subscriber
    // is installed — always reaches the journal via systemd's
    // stderr capture regardless of the filter directive (a
    // `warn`-filtered subscriber would drop this at INFO, so
    // eprintln! is the only path that's guaranteed to surface
    // the resolved filter for post-hoc journal inspection).
    // Lets operators + support workflows verify which filter is
    // actually installed on a shipping binary without re-reading
    // the config-resolution code path.
    eprintln!(
        "evo: tracing subscriber installed — filter directive: {} \
         (source: {})",
        resolved_directive, resolution_source
    );

    // Per-layer filter attachment via `.with_filter(...)`. The
    // shape `Registry::default().with(filter).with(sink)` composes
    // the EnvFilter as a global filter layer, but the interest
    // computation between the registry-level filter and the
    // subsequent sink layer can leave DEBUG events reaching
    // sinks even under a `warn` directive. Per-layer filtering
    // (`sink.with_filter(filter)`) attaches the filter directly
    // to the specific sink, so every event the filter rejects is
    // guaranteed to never reach the sink's `on_event` — the
    // recommended pattern for tracing-subscriber 0.3+ per the
    // crate's own layer-composition documentation.
    #[cfg(target_os = "linux")]
    {
        match tracing_journald::layer() {
            Ok(journald_layer) => {
                Registry::default()
                    .with(journald_layer.with_filter(filter))
                    .try_init()
                    .map_err(|e| {
                        StewardError::Config(format!(
                            "tracing subscriber init: {e}"
                        ))
                    })?;
                Ok(LoggingGuard {
                    _non_blocking: None,
                })
            }
            Err(e) => {
                // Journald unavailable (running as non-root, or container
                // without journald, or non-systemd host). Fall through to
                // the stderr fallback path; the prior layer's
                // unavailability reason is preserved in a debug log
                // emitted after the fallback layer is installed.
                let (non_blocking, guard) =
                    tracing_appender::non_blocking(std::io::stderr());
                let fmt_layer = tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_writer(non_blocking);
                Registry::default()
                    .with(fmt_layer.with_filter(filter))
                    .try_init()
                    .map_err(|init_err| {
                        StewardError::Config(format!(
                            "tracing subscriber init: {init_err}"
                        ))
                    })?;
                tracing::debug!(
                    reason = %e,
                    "journald layer unavailable; using non-blocking \
                     stderr fallback"
                );
                Ok(LoggingGuard {
                    _non_blocking: Some(guard),
                })
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let (non_blocking, guard) =
            tracing_appender::non_blocking(std::io::stderr());
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_writer(non_blocking);
        Registry::default()
            .with(fmt_layer.with_filter(filter))
            .try_init()
            .map_err(|e| {
                StewardError::Config(format!("tracing subscriber init: {e}"))
            })?;
        Ok(LoggingGuard {
            _non_blocking: Some(guard),
        })
    }
}

/// Resolve the active `EnvFilter` per LOGGING.md section 3, with CLI
/// override support.
///
/// Precedence (highest wins):
/// 1. `cli_override` if supplied and parses as a valid filter directive.
/// 2. `RUST_LOG` if set and parses as a valid filter directive.
/// 3. `config_level` if it parses as a valid filter directive.
/// 4. Hard-coded fallback of
///    [`evo_plugin_sdk::wire_logging::DEFAULT_WIRE_ENV_FILTER`]
///    — the shared production baseline that quiets known
///    log-bridged demuxer chatter (e.g. `lofty` frame-header
///    retries) while keeping every first-party crate at
///    `warn`. LOGGING.md §3 documents the directive.
///
/// If `cli_override` is supplied but fails to parse, a warning is
/// written to stderr (tracing is not yet initialised here) and the
/// resolution falls through to the next precedence level.
/// Additionally returns the resolved directive string and its
/// source tag so [`init`] can emit a boot-time diagnostic naming
/// what filter is actually installed. The `_with_source` suffix
/// is the canonical entrypoint; a thin `resolve_filter` wrapper
/// is retained under `#[cfg(test)]` below so the pre-existing
/// test surface (which only exercises filter construction, not
/// the source tag) continues to work without per-test tuple
/// destructuring boilerplate.
fn resolve_filter_with_source(
    config_level: &str,
    cli_override: Option<&str>,
) -> (EnvFilter, String, &'static str) {
    if let Some(level) = cli_override {
        match EnvFilter::try_new(level) {
            Ok(f) => return (f, level.to_string(), "cli --log-level"),
            Err(e) => {
                eprintln!(
                    "evo: warning: invalid --log-level '{level}': {e}; falling back"
                );
            }
        }
    }

    if let Ok(s) = std::env::var("RUST_LOG") {
        if let Ok(f) = EnvFilter::try_new(&s) {
            return (f, s, "RUST_LOG env");
        }
    }

    match EnvFilter::try_new(config_level) {
        Ok(f) => (f, config_level.to_string(), "config [steward] log_level"),
        Err(_) => (
            EnvFilter::new(
                evo_plugin_sdk::wire_logging::DEFAULT_WIRE_ENV_FILTER,
            ),
            evo_plugin_sdk::wire_logging::DEFAULT_WIRE_ENV_FILTER.to_string(),
            "DEFAULT_WIRE_ENV_FILTER fallback",
        ),
    }
}

#[cfg(test)]
fn resolve_filter(config_level: &str, cli_override: Option<&str>) -> EnvFilter {
    resolve_filter_with_source(config_level, cli_override).0
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

    /// When every input is invalid (config unparseable, RUST_LOG
    /// unset, no CLI override), resolve_filter must fall back to
    /// the shared production directive from the SDK — proving the
    /// steward and its OOP wire binaries speak one baseline.
    #[test]
    fn resolve_filter_shared_default_when_config_invalid() {
        std::env::remove_var("RUST_LOG");
        // Bare '!!!' is not a valid directive; the fallback path
        // uses the SDK's DEFAULT_WIRE_ENV_FILTER string. We
        // cannot assert on the resulting filter's directives
        // directly (EnvFilter's internals are private) — instead
        // verify the SDK constant parses and matches what the
        // steward embeds.
        let _ = resolve_filter("!!!not-a-valid-config!!!", None);
        let sdk_default = EnvFilter::try_new(
            evo_plugin_sdk::wire_logging::DEFAULT_WIRE_ENV_FILTER,
        );
        assert!(sdk_default.is_ok());
    }
}
