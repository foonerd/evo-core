//! Error types for the acceptance harness.

use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
pub enum HarnessError {
    DescriptorRead {
        path: PathBuf,
        source: io::Error,
    },
    DescriptorParse {
        path: PathBuf,
        source: toml::de::Error,
    },
    DescriptorPlaceholder {
        path: PathBuf,
        fields: Vec<String>,
        overlay_path: PathBuf,
    },
    PlanRead {
        path: PathBuf,
        source: io::Error,
    },
    PlanParse {
        path: PathBuf,
        source: toml::de::Error,
    },
    ConnectionFailed {
        detail: String,
    },
    CommandSpawn {
        command: String,
        source: io::Error,
    },
    InvalidRegex {
        pattern: String,
        source: regex::Error,
    },
    ReportWrite {
        path: PathBuf,
        source: io::Error,
    },
    UnknownConnectionType {
        value: String,
    },
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DescriptorRead { path, source } => {
                write!(
                    f,
                    "could not read target descriptor at {path:?}: {source}"
                )
            }
            Self::DescriptorParse { path, source } => {
                write!(
                    f,
                    "could not parse target descriptor at {path:?}: {source}"
                )
            }
            Self::DescriptorPlaceholder {
                path,
                fields,
                overlay_path,
            } => {
                write!(
                    f,
                    "target descriptor at {path:?} still carries the \"REPLACE_ME\" \
                     placeholder in [{fields}]; supply the connection details in the local \
                     overlay file at {overlay_path:?} (gitignored, see \
                     acceptance/README.md §Local overrides)",
                    fields = fields.join(", "),
                )
            }
            Self::PlanRead { path, source } => {
                write!(f, "could not read scenario plan at {path:?}: {source}")
            }
            Self::PlanParse { path, source } => {
                write!(f, "could not parse scenario plan at {path:?}: {source}")
            }
            Self::ConnectionFailed { detail } => {
                write!(f, "target connection failed: {detail}")
            }
            Self::CommandSpawn { command, source } => {
                write!(f, "could not spawn command {command:?}: {source}")
            }
            Self::InvalidRegex { pattern, source } => {
                write!(f, "invalid pass-criterion regex {pattern:?}: {source}")
            }
            Self::ReportWrite { path, source } => {
                write!(
                    f,
                    "could not write readiness report to {path:?}: {source}"
                )
            }
            Self::UnknownConnectionType { value } => {
                write!(
                    f,
                    "unknown connection_type {value:?} in target descriptor"
                )
            }
        }
    }
}

impl std::error::Error for HarnessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DescriptorRead { source, .. }
            | Self::PlanRead { source, .. }
            | Self::CommandSpawn { source, .. }
            | Self::ReportWrite { source, .. } => Some(source),
            Self::DescriptorParse { source, .. }
            | Self::PlanParse { source, .. } => Some(source),
            Self::InvalidRegex { source, .. } => Some(source),
            Self::ConnectionFailed { .. }
            | Self::DescriptorPlaceholder { .. }
            | Self::UnknownConnectionType { .. } => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, HarnessError>;
