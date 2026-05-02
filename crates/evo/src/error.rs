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

    /// Wrapped persistence-store error.
    #[error("persistence error: {0}")]
    Persistence(#[from] crate::persistence::PersistenceError),

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
    /// The mapping is deterministic and stable so consumers reading
    /// [`ApiError::class`] on the wire can rely on it.
    ///
    /// Two cases defer to richer downstream classifiers rather than
    /// collapsing to a single class:
    ///
    /// - [`Self::Plugin`] forwards to
    ///   [`PluginError::class`](evo_plugin_sdk::contract::PluginError::class)
    ///   so transient, permission-denied, resource-exhausted, and
    ///   internal plugin errors keep their distinct semantics across
    ///   the boundary instead of all surfacing as `Internal`.
    /// - [`Self::Dispatch`] is split by its message prefix via
    ///   [`classify_dispatch`] so the router's three failure modes
    ///   (no plugin on shelf, plugin unloaded, contract failure)
    ///   land on `NotFound`, `Unavailable`, and `ContractViolation`
    ///   respectively. The prefix match is keyed on the strings the
    ///   router emits today; restructuring `Dispatch` into multiple
    ///   variants would be a wider refactor for the same observable
    ///   class outcome.
    pub fn classify(&self) -> ErrorClass {
        match self {
            Self::Config(_) => ErrorClass::Misconfiguration,
            Self::Catalogue(_) => ErrorClass::Misconfiguration,
            Self::Admission(_) => ErrorClass::ContractViolation,
            Self::Dispatch(msg) => classify_dispatch(msg),
            Self::Io { .. } => ErrorClass::Internal,
            Self::Manifest(_) => ErrorClass::Misconfiguration,
            Self::Plugin(e) => e.class(),
            Self::Toml { .. } => ErrorClass::Misconfiguration,
            Self::AdminTrustTooLow { .. } => ErrorClass::TrustViolation,
            Self::Persistence(_) => ErrorClass::Internal,
        }
    }
}

/// Classify a [`StewardError::Dispatch`] message into its
/// [`ErrorClass`].
///
/// Three failure modes are surfaced from the router today:
///
/// - `"no plugin on shelf: ..."` — the shelf carries no admitted
///   plugin. Maps to [`ErrorClass::NotFound`].
/// - `"plugin on shelf X has been unloaded"` — the shelf had a
///   plugin but it has been unloaded. Maps to
///   [`ErrorClass::Unavailable`] (transient at the operational
///   level; a future load may restore service).
/// - Anything else (kind mismatch, validation failure, internal
///   write failures the router wraps in `Dispatch`) — maps to
///   [`ErrorClass::ContractViolation`], the conservative default
///   that preserves the previous observable behaviour for
///   non-routing dispatch failures.
///
/// Match keys are prefixes deliberately: callers may append further
/// detail (a shelf name, a plugin name) without invalidating the
/// classification. The strings themselves are emitted by
/// [`crate::router`]; a router-side change to either string MUST
/// be paired with a change here, kept honest by the unit tests.
pub fn classify_dispatch(msg: &str) -> ErrorClass {
    if msg.starts_with("no plugin on shelf:") {
        ErrorClass::NotFound
    } else if msg.starts_with("plugin on shelf ")
        && msg.contains(" has been unloaded")
    {
        ErrorClass::Unavailable
    } else {
        ErrorClass::ContractViolation
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

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::PluginError;

    // ----- Step 1: StewardError::Plugin defers to PluginError::class -----
    //
    // Before the change every Plugin variant collapsed to
    // ErrorClass::Internal regardless of whether the underlying
    // plugin error was transient, permission-denied, or
    // resource-exhausted. The collapse erased information consumers
    // need to drive retry, escalation, and circuit-breaker decisions.

    #[test]
    fn steward_plugin_transient_classifies_as_transient() {
        let e = StewardError::Plugin(PluginError::Transient("blip".into()));
        assert_eq!(e.classify(), ErrorClass::Transient);
    }

    #[test]
    fn steward_plugin_permanent_classifies_as_contract_violation() {
        let e =
            StewardError::Plugin(PluginError::Permanent("bad input".into()));
        assert_eq!(e.classify(), ErrorClass::ContractViolation);
    }

    #[test]
    fn steward_plugin_unauthorized_classifies_as_permission_denied() {
        let e = StewardError::Plugin(PluginError::Unauthorized(
            "need user auth".into(),
        ));
        assert_eq!(e.classify(), ErrorClass::PermissionDenied);
    }

    #[test]
    fn steward_plugin_timeout_classifies_as_transient() {
        let e = StewardError::Plugin(PluginError::Timeout {
            elapsed_ms: 5000,
            deadline_ms: 3000,
        });
        assert_eq!(e.classify(), ErrorClass::Transient);
    }

    #[test]
    fn steward_plugin_resource_exhausted_classifies_as_resource_exhausted() {
        let e = StewardError::Plugin(PluginError::ResourceExhausted {
            resource: "memory".into(),
        });
        assert_eq!(e.classify(), ErrorClass::ResourceExhausted);
    }

    #[test]
    fn steward_plugin_internal_classifies_as_internal() {
        let io_err = std::io::Error::other("oops");
        let e = StewardError::Plugin(PluginError::internal("ctx", io_err));
        assert_eq!(e.classify(), ErrorClass::Internal);
    }

    #[test]
    fn steward_plugin_fatal_classifies_as_internal() {
        let io_err = std::io::Error::other("disk gone");
        let e = StewardError::Plugin(PluginError::fatal("writing", io_err));
        assert_eq!(e.classify(), ErrorClass::Internal);
    }

    // ----- Step 2: StewardError::Dispatch is split by message prefix -----

    #[test]
    fn dispatch_no_plugin_on_shelf_classifies_as_not_found() {
        let e =
            StewardError::Dispatch("no plugin on shelf: org.foo.bar".into());
        assert_eq!(e.classify(), ErrorClass::NotFound);
    }

    #[test]
    fn dispatch_plugin_unloaded_classifies_as_unavailable() {
        let e = StewardError::Dispatch(
            "plugin on shelf org.foo.bar has been unloaded".into(),
        );
        assert_eq!(e.classify(), ErrorClass::Unavailable);
    }

    #[test]
    fn dispatch_other_classifies_as_contract_violation() {
        // Examples emitted by the router today: kind-mismatch
        // ("warden received respondent verb"), validation failure,
        // internal write-failure wrapping. None of these are
        // routing-not-found or routing-unavailable; they remain
        // ContractViolation.
        let e =
            StewardError::Dispatch("warden received respondent verb".into());
        assert_eq!(e.classify(), ErrorClass::ContractViolation);

        let e = StewardError::Dispatch(
            "happenings_log write failed: disk full".into(),
        );
        assert_eq!(e.classify(), ErrorClass::ContractViolation);

        let e = StewardError::Dispatch("validation refused".into());
        assert_eq!(e.classify(), ErrorClass::ContractViolation);
    }

    #[test]
    fn classify_dispatch_helper_matches_prefixes_only() {
        // Helper is exposed for direct router-side use; sanity-check
        // the prefix discipline so a router-side message change
        // surfaces here at test time.
        assert_eq!(
            classify_dispatch("no plugin on shelf: x"),
            ErrorClass::NotFound
        );
        assert_eq!(
            classify_dispatch("plugin on shelf x has been unloaded"),
            ErrorClass::Unavailable
        );
        // Substring elsewhere in the message must NOT trigger.
        assert_eq!(
            classify_dispatch("validation refused: no plugin on shelf"),
            ErrorClass::ContractViolation
        );
    }

    // ----- ApiError envelope reflects the new classes -----

    #[test]
    fn api_error_carries_plugin_class_through_envelope() {
        let e = StewardError::Plugin(PluginError::Transient("blip".into()));
        let api: ApiError = (&e).into();
        assert_eq!(api.class, ErrorClass::Transient);
    }

    #[test]
    fn api_error_carries_dispatch_not_found_through_envelope() {
        let e = StewardError::Dispatch("no plugin on shelf: x".into());
        let api: ApiError = (&e).into();
        assert_eq!(api.class, ErrorClass::NotFound);
    }

    #[test]
    fn api_error_carries_dispatch_unavailable_through_envelope() {
        let e = StewardError::Dispatch(
            "plugin on shelf x has been unloaded".into(),
        );
        let api: ApiError = (&e).into();
        assert_eq!(api.class, ErrorClass::Unavailable);
    }
}
