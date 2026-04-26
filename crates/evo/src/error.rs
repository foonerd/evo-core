//! Steward error type.

use crate::error_taxonomy::{ApiError, ErrorClass};
use evo_plugin_sdk::manifest::TrustClass;
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

    /// A plugin declared `capabilities.admin = true` but its
    /// effective trust class is below the admin minimum. Raised by
    /// [`AdmissionEngine::check_admin_trust`](crate::admission::AdmissionEngine)
    /// at every admit entry point after effective-class
    /// computation.
    ///
    /// The carried values let the operator see at a glance which
    /// class the plugin received versus the minimum the steward
    /// demands for admin plugins.
    #[error(
        "admin trust too low: plugin '{plugin_name}' effective class \
         {effective:?} is below admin minimum {minimum:?}"
    )]
    AdminTrustTooLow {
        /// Canonical plugin name, for operator diagnostics.
        plugin_name: String,
        /// The effective trust class the admission engine computed
        /// for this plugin (after signature-and-key gating).
        effective: TrustClass,
        /// The admin minimum the steward requires
        /// ([`evo_trust::ADMIN_MINIMUM_TRUST`]).
        minimum: TrustClass,
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

    /// Map a [`StewardError`] to its [`ErrorClass`].
    ///
    /// Heuristic, not contractual: variants whose semantics will be
    /// rebased onto a richer internal taxonomy in a future pass
    /// currently land on the closest broad class. The mapping is
    /// deterministic and stable so consumers reading
    /// [`ApiError::class`] on the wire can rely on it.
    pub fn classify(&self) -> ErrorClass {
        match self {
            Self::Config(_) => ErrorClass::Misconfiguration,
            Self::Catalogue(_) => ErrorClass::Misconfiguration,
            Self::Admission(_) => ErrorClass::ContractViolation,
            Self::Dispatch(_) => ErrorClass::ContractViolation,
            Self::Io { .. } => ErrorClass::Internal,
            Self::Manifest(_) => ErrorClass::Misconfiguration,
            Self::Plugin(_) => ErrorClass::Internal,
            Self::Toml { .. } => ErrorClass::Misconfiguration,
            Self::AdminTrustTooLow { .. } => ErrorClass::TrustViolation,
        }
    }
}

impl From<&StewardError> for ApiError {
    fn from(e: &StewardError) -> Self {
        let mut err = ApiError::new(e.classify(), e.to_string());
        // Surface structured fields on classes that already carry
        // them so consumers can act without parsing the message.
        if let StewardError::AdminTrustTooLow {
            plugin_name,
            effective,
            minimum,
        } = e
        {
            err = err.with_details(serde_json::json!({
                "subclass": "admin_trust_too_low",
                "plugin_name": plugin_name,
                "effective": format!("{effective:?}"),
                "minimum": format!("{minimum:?}"),
            }));
        }
        err
    }
}

impl From<StewardError> for ApiError {
    fn from(e: StewardError) -> Self {
        (&e).into()
    }
}
