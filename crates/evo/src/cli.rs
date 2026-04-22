//! Command-line argument parsing.
//!
//! The evo binary accepts a small set of flags that override values from
//! the config file. Flag precedence (highest first):
//!
//! - `--log-level LEVEL` wins over `RUST_LOG` and
//!   `config.steward.log_level`.
//! - `RUST_LOG` wins over `config.steward.log_level`.
//! - `config.steward.log_level` wins over the hardcoded fallback `warn`.
//!
//! For path flags (`--catalogue`, `--socket`), the CLI value simply
//! replaces the corresponding config value if given.
//!
//! The `--config PATH` flag overrides the location the config file is
//! read from. When given, a missing file is an error (because the user
//! asked for a specific file); without it, a missing default config is
//! silently replaced by built-in defaults.

use clap::Parser;
use std::path::PathBuf;

/// The evo steward binary.
///
/// Administers a catalogue, admits plugins, and serves client requests
/// over a Unix domain socket.
#[derive(Debug, Parser)]
#[command(name = "evo", version, about, long_about = None)]
pub struct Args {
    /// Path to the steward config file.
    ///
    /// Default: /etc/evo/evo.toml. When set and the file does not exist,
    /// startup fails.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Path to the catalogue TOML file.
    ///
    /// Overrides the `catalogue.path` value from the config file.
    #[arg(long, value_name = "PATH")]
    pub catalogue: Option<PathBuf>,

    /// Unix socket path for the steward to bind.
    ///
    /// Overrides the `steward.socket_path` value from the config file.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Log level filter.
    ///
    /// One of: error, warn, info, debug, trace. Also accepts
    /// target-specific directives like `evo=info,tokio=warn`. Takes
    /// precedence over RUST_LOG and over the config file.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,
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
        let args = Args::try_parse_from(["evo", "--config", "/etc/evo.toml"]).unwrap();
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
}
