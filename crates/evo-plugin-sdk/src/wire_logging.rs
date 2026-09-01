// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Standard tracing-subscriber setup for out-of-process plugin
//! wire binaries.
//!
//! Every `*-wire` binary in the reference plugin set installed a
//! byte-identical `init_logging` helper that:
//!
//! - built an [`EnvFilter`] from `RUST_LOG` with a bare `warn`
//!   fallback,
//! - installed a `tracing_subscriber::fmt` layer writing to
//!   stderr with `with_target(false)`.
//!
//! Two problems with that shape:
//!
//! 1. A bare `warn` default admits WARN-level records from any
//!    dependency logging via the `log` crate (bridged by
//!    `tracing_log::LogTracer`, which
//!    `tracing_subscriber::fmt::init` installs). Media-tag
//!    parsers like `lofty` are recoverable-error-chatty on
//!    real-world MP3 libraries — their `warn!("Failed to parse
//!    a frame ID: …")` messages are frame-header retries the
//!    parser handles, not operator-actionable faults. Per
//!    `docs/engineering/LOGGING.md` §2, those messages are not
//!    warn. The steward's own default in
//!    `evo::logging::resolve_filter` had the same problem.
//!
//! 2. The 8 lines duplicated across every wire binary meant
//!    tuning the default (quieting a newly noisy dep, changing
//!    the format, adding a target list) required a per-binary
//!    sweep.
//!
//! [`init`] is the shared installer — one call, replaces the
//! duplicated boilerplate, and honours [`DEFAULT_WIRE_ENV_FILTER`]
//! as the baseline when `RUST_LOG` is unset. Operators wanting
//! per-target detail set `RUST_LOG` verbatim (e.g.
//! `RUST_LOG=lofty=warn` to restore the demuxer chatter for tag
//! triage); the RUST_LOG override wins over the baked default.

use tracing_subscriber::EnvFilter;

/// Baseline `EnvFilter` directive applied when no `RUST_LOG`
/// override is set.
///
/// - `warn` — first-party evo / plugin targets at WARN and
///   above, matching LOGGING.md §3's production default.
/// - `lofty=error` — the audio-tag parser's recoverable
///   frame-header retries + bitrate-duration estimates are
///   normal for real-world MP3 libraries; per LOGGING.md §2
///   they are not warn. Errors from lofty (parse-refused,
///   IO-refused) still surface. Extend this directive as new
///   noisy log-bridged dependencies surface — the change lands
///   in one place and every steward + wire process picks it up.
///
/// Passing a bare level via `RUST_LOG=info` or `RUST_LOG=warn`
/// still overrides this default in full — the baseline exists
/// so operators who set nothing get the quiet-by-design
/// production surface, and operators debugging a specific
/// dependency set the directive they want.
pub const DEFAULT_WIRE_ENV_FILTER: &str = "warn,lofty=error";

/// Install the shared out-of-process wire tracing subscriber.
///
/// Filter resolution:
///
/// 1. `RUST_LOG` verbatim if set and parseable.
/// 2. [`DEFAULT_WIRE_ENV_FILTER`].
///
/// Emission layer: a `tracing_subscriber::fmt` layer writing to
/// stderr with `with_target(false)`. The `tracing-journald`
/// path is exclusive to the in-process steward — OOP plugins
/// funnel their events back to the steward through the wire
/// protocol's `log_event` message (per LOGGING.md §8), and
/// stderr is systemd's default capture surface for the wire
/// process itself while a wire log-event surface lands.
///
/// This function must be called exactly once, before any
/// `tracing::*` macro is invoked. Subsequent calls will panic
/// via `tracing_subscriber::fmt::init`'s
/// `SetGlobalDefaultError` — matches the shape wire binaries
/// had before this helper landed.
pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_WIRE_ENV_FILTER));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The baseline directive must parse — a typo here would
    /// crash every wire binary the moment it started.
    #[test]
    fn default_directive_is_a_valid_env_filter() {
        let _f = EnvFilter::new(DEFAULT_WIRE_ENV_FILTER);
    }

    /// The baseline must actually contain both the first-party
    /// warn floor and the lofty quiet directive. This guards
    /// against a future editor stripping one half by accident.
    #[test]
    fn default_directive_carries_first_party_warn_and_lofty_error() {
        assert!(DEFAULT_WIRE_ENV_FILTER.contains("warn"));
        assert!(DEFAULT_WIRE_ENV_FILTER.contains("lofty=error"));
    }
}
