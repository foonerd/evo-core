// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Command-line argument parsing.
//!
//! The evo binary has two modes. Default invocation `evo [flags]`
//! boots the steward; the `evo audit lifecycle [...]` subcommand
//! runs an operator-facing read-only query against the audit-grade
//! ledger and exits without booting.
//!
//! ## Boot-flag precedence
//!
//! Flags override config-file values (highest first):
//! `--log-level LEVEL` wins over `RUST_LOG` and over
//! `config.steward.log_level`; `RUST_LOG` wins over
//! `config.steward.log_level`; `config.steward.log_level` wins
//! over the hardcoded fallback `warn`. For path flags
//! (`--catalogue`, `--socket`), the CLI value simply replaces the
//! corresponding config value if given. The `--config PATH` flag
//! overrides the location the config file is read from; when
//! given, a missing file is an error.
//!
//! ## Audit subcommand
//!
//! `evo audit lifecycle [...]` opens the steward's persistence
//! database read-only, filters by plugin / time range / event
//! type, and prints rows in causal order. Runs concurrently with
//! a live steward via SQLite WAL.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// The evo steward binary.
///
/// Default invocation (no subcommand) administers a catalogue, admits
/// plugins, and serves client requests over a Unix domain socket. The
/// `audit` subcommand exposes operator-facing read-only queries
/// against the audit-grade ledger.
#[derive(Debug, Parser)]
#[command(name = "evo", version, about, long_about = None)]
pub struct Args {
    /// Path to the steward config file.
    ///
    /// Default: /etc/evo/evo.toml. When set and the file does not exist,
    /// startup fails.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Path to the catalogue TOML file.
    ///
    /// Overrides the `catalogue.path` value from the config file.
    #[arg(long, value_name = "PATH", global = true)]
    pub catalogue: Option<PathBuf>,

    /// Unix socket path for the steward to bind.
    ///
    /// Overrides the `steward.socket_path` value from the config file.
    #[arg(long, value_name = "PATH", global = true)]
    pub socket: Option<PathBuf>,

    /// Log level filter.
    ///
    /// One of: error, warn, info, debug, trace. Also accepts
    /// target-specific directives like `evo=info,tokio=warn`. Takes
    /// precedence over RUST_LOG and over the config file.
    #[arg(long, value_name = "LEVEL", global = true)]
    pub log_level: Option<String>,

    /// Optional subcommand. When omitted, the binary boots the
    /// steward.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level subcommands. Default invocation (no subcommand) boots
/// the steward.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Forensic-query commands against the audit-grade ledger.
    Audit {
        /// Audit subcommand selector.
        #[command(subcommand)]
        audit_command: AuditCommand,
    },
}

/// Audit-subcommand selector.
#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// Query the `evo.lifecycle` ledger track for plugin-initiated
    /// primitive lifecycle events (streams open/close, notifications
    /// send/cancel, metadata query dispatch / provider failure /
    /// provider timeout). Reads the steward's persistence database
    /// read-only via SQLite WAL; safe to run concurrently with a
    /// live steward.
    Lifecycle {
        /// Path to the steward's persistence database. Defaults to
        /// `/var/lib/evo/evo.db` to match the standard install
        /// location; override for non-default installs.
        #[arg(
            long,
            value_name = "PATH",
            default_value = "/var/lib/evo/evo.db"
        )]
        db: PathBuf,

        /// Restrict results to entries whose `source_plugin`
        /// matches.
        #[arg(long, value_name = "PLUGIN_ID")]
        plugin: Option<String>,

        /// Restrict results to entries with
        /// `recorded_at_ms >= since` (Unix milliseconds).
        #[arg(long, value_name = "MS")]
        since: Option<u64>,

        /// Restrict results to entries with
        /// `recorded_at_ms <= until` (Unix milliseconds).
        #[arg(long, value_name = "MS")]
        until: Option<u64>,

        /// Restrict to a single event type. One of: `stream_opened`,
        /// `stream_closed`, `notification_sent`,
        /// `notification_cancelled`, `metadata_query_dispatched`,
        /// `metadata_provider_timed_out`,
        /// `metadata_provider_failed`.
        #[arg(long, value_name = "TYPE")]
        event_type: Option<String>,

        /// Maximum number of rows to return.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Include withdrawn entries. Default omits them so the
        /// human-readable output reflects the current view.
        #[arg(long, default_value_t = false)]
        include_withdrawn: bool,

        /// Output format. One of: `human` (default), `tsv`, `json`.
        #[arg(long, value_name = "FORMAT", default_value = "human")]
        format: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_with_no_flags() {
        let args = Args::try_parse_from(["evo"]).unwrap();
        assert!(args.config.is_none());
        assert!(args.catalogue.is_none());
        assert!(args.socket.is_none());
        assert!(args.log_level.is_none());
    }

    #[test]
    fn parses_all_flags() {
        let args = Args::try_parse_from([
            "evo",
            "--config",
            "/tmp/evo.toml",
            "--catalogue",
            "/tmp/cat.toml",
            "--socket",
            "/tmp/evo.sock",
            "--log-level",
            "debug",
        ])
        .unwrap();
        assert_eq!(args.config.as_deref(), Some(Path::new("/tmp/evo.toml")));
        assert_eq!(args.catalogue.as_deref(), Some(Path::new("/tmp/cat.toml")));
        assert_eq!(args.socket.as_deref(), Some(Path::new("/tmp/evo.sock")));
        assert_eq!(args.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn parses_config_only() {
        let args =
            Args::try_parse_from(["evo", "--config", "/etc/evo.toml"]).unwrap();
        assert_eq!(args.config.as_deref(), Some(Path::new("/etc/evo.toml")));
        assert!(args.catalogue.is_none());
    }

    #[test]
    fn rejects_unknown_flag() {
        let r = Args::try_parse_from(["evo", "--nonexistent", "foo"]);
        assert!(r.is_err());
    }

    #[test]
    fn rejects_missing_flag_value() {
        let r = Args::try_parse_from(["evo", "--config"]);
        assert!(r.is_err());
    }

    #[test]
    fn version_flag_exits_with_version() {
        // clap returns DisplayVersion kind on --version. We just check
        // that it doesn't parse as normal args.
        let r = Args::try_parse_from(["evo", "--version"]);
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn help_flag_exits_with_help() {
        let r = Args::try_parse_from(["evo", "--help"]);
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn no_subcommand_means_steward_boot() {
        let args = Args::try_parse_from(["evo"]).unwrap();
        assert!(args.command.is_none());
    }

    #[test]
    fn audit_lifecycle_subcommand_parses_with_defaults() {
        let args = Args::try_parse_from(["evo", "audit", "lifecycle"]).unwrap();
        match args.command {
            Some(Command::Audit {
                audit_command:
                    AuditCommand::Lifecycle {
                        db,
                        plugin,
                        since,
                        until,
                        event_type,
                        limit,
                        include_withdrawn,
                        format,
                    },
            }) => {
                assert_eq!(db, Path::new("/var/lib/evo/evo.db"));
                assert!(plugin.is_none());
                assert!(since.is_none());
                assert!(until.is_none());
                assert!(event_type.is_none());
                assert!(limit.is_none());
                assert!(!include_withdrawn);
                assert_eq!(format, "human");
            }
            other => {
                panic!("expected Audit::Lifecycle subcommand, got {other:?}")
            }
        }
    }

    #[test]
    fn audit_lifecycle_accepts_every_filter() {
        let args = Args::try_parse_from([
            "evo",
            "audit",
            "lifecycle",
            "--db",
            "/tmp/audit.db",
            "--plugin",
            "org.test",
            "--since",
            "1000",
            "--until",
            "2000",
            "--event-type",
            "stream_opened",
            "--limit",
            "50",
            "--include-withdrawn",
            "--format",
            "json",
        ])
        .unwrap();
        match args.command {
            Some(Command::Audit {
                audit_command:
                    AuditCommand::Lifecycle {
                        db,
                        plugin,
                        since,
                        until,
                        event_type,
                        limit,
                        include_withdrawn,
                        format,
                    },
            }) => {
                assert_eq!(db, Path::new("/tmp/audit.db"));
                assert_eq!(plugin.as_deref(), Some("org.test"));
                assert_eq!(since, Some(1000));
                assert_eq!(until, Some(2000));
                assert_eq!(event_type.as_deref(), Some("stream_opened"));
                assert_eq!(limit, Some(50));
                assert!(include_withdrawn);
                assert_eq!(format, "json");
            }
            other => {
                panic!("expected Audit::Lifecycle subcommand, got {other:?}")
            }
        }
    }
}
