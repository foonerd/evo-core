//! Steward error type.

use thiserror::Error;

/// Errors produced by the steward.
///
/// Variants are coarse at this stage. As the steward matures, finer-grained
/// error variants will replace generic strings where useful for callers.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StewardError {
    /// Configuration loading or validation failed.
    #[error("configuration error: {0}")]
    Config(String),

    /// Catalogue loading or validation failed.
    #[error("catalogue error: {0}")]
    Catalogue(String),

    /// A plugin could not be admitted. Carries a plugin-name-and-reason
    /// string.
    #[error("admission error: {0}")]
    Admission(String),

    /// A client request could not be dispatched.
    #[error("dispatch error: {0}")]
    Dispatch(String),

    /// Wrapped I/O error with a context string describing what was being
    /// attempted.
    #[error("I/O error ({context}): {source}")]
    Io {
        /// What the steward was trying to do when the error occurred.
        context: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Wrapped plugin SDK manifest error.
    #[error("manifest error: {0}")]
    Manifest(#[from] evo_plugin_sdk::ManifestError),

    /// Wrapped plugin contract error.
    #[error("plugin error: {0}")]
    Plugin(#[from] evo_plugin_sdk::contract::PluginError),

    /// Wrapped TOML parse error.
    #[error("TOML parse error ({context}): {source}")]
    Toml {
        /// What file or content was being parsed.
        context: String,
        /// The underlying TOML error.
        #[source]
        source: toml::de::Error,
    },
}

impl StewardError {
    /// Helper: construct an I/O error with a context string.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Helper: construct a TOML parse error with a context string.
    pub fn toml(context: impl Into<String>, source: toml::de::Error) -> Self {
        Self::Toml {
            context: context.into(),
            source,
        }
    }
}
