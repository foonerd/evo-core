//! Plugin error types.
//!
//! [`PluginError`] is the canonical error returned by every plugin contract
//! verb. It distinguishes failure modes the steward must handle differently:
//! transient errors may be retried, permanent errors will not succeed on
//! retry, fatal errors cause the plugin to be deregistered.

use std::error::Error as StdError;
use thiserror::Error;

/// Canonical error returned by plugin contract verbs.
///
/// Variants are classified along two axes:
///
/// - **Severity**: is the plugin still viable after this error? Use
///   [`PluginError::is_fatal`] to check.
/// - **Transience**: will a retry succeed? Use [`PluginError::is_transient`]
///   to check.
///
/// The steward uses these classifications to drive backoff, circuit
/// breakers, custody transfer, and deregistration.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PluginError {
    /// A transient error. The plugin is operational; retry may succeed.
    /// Typical causes: upstream service blip, lock contention, transient
    /// network failure.
    #[error("transient error: {0}")]
    Transient(String),

    /// A permanent error. The plugin is operational; retry will not
    /// succeed. Typical causes: bad input from the steward, missing
    /// requested resource, unsupported request type for this plugin's
    /// current runtime capabilities.
    #[error("permanent error: {0}")]
    Permanent(String),

    /// The request requires authentication the plugin cannot complete
    /// without user interaction. The plugin should have already emitted a
    /// `request_user_interaction` before or alongside this error.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The plugin could not complete the verb within the required deadline.
    #[error("timeout: elapsed {elapsed_ms}ms, deadline {deadline_ms}ms")]
    Timeout {
        /// Elapsed time when the plugin gave up.
        elapsed_ms: u64,
        /// Deadline the steward imposed or the plugin agreed to honour.
        deadline_ms: u64,
    },

    /// A declared resource (memory, CPU budget, open file limit, external
    /// connection pool, etc.) is exhausted.
    #[error("resource exhausted: {resource}")]
    ResourceExhausted {
        /// Name of the exhausted resource, for log and metric correlation.
        resource: String,
    },

    /// An internal error the plugin can recover from. The plugin is still
    /// operational. Carries both a source error and a context string
    /// describing what the plugin was trying to do.
    #[error("internal error ({context}): {source}")]
    Internal {
        /// What the plugin was trying to do when this error occurred.
        context: String,
        /// The underlying error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },

    /// A fatal error. The plugin cannot continue. The steward will
    /// deregister it. Wardens holding custody: the custody is considered
    /// released in an undefined state; the steward notifies consumers.
    #[error("fatal error ({context}): {source}")]
    Fatal {
        /// What the plugin was trying to do when this error occurred.
        context: String,
        /// The underlying error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl PluginError {
    /// Returns true if this error is fatal and requires plugin
    /// deregistration.
    pub fn is_fatal(&self) -> bool {
        matches!(self, PluginError::Fatal { .. })
    }

    /// Returns true if this error is transient and a retry may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, PluginError::Transient(_))
    }

    /// Returns true if this error is permanent: not transient, not fatal.
    /// A retry will not succeed but the plugin remains operational.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            PluginError::Permanent(_)
                | PluginError::Unauthorized(_)
                | PluginError::Timeout { .. }
                | PluginError::ResourceExhausted { .. }
                | PluginError::Internal { .. }
        )
    }

    /// Construct an `Internal` error with a context string and a source.
    pub fn internal<E, C>(context: C, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
        C: Into<String>,
    {
        PluginError::Internal {
            context: context.into(),
            source: Box::new(source),
        }
    }

    /// Construct a `Fatal` error with a context string and a source.
    pub fn fatal<E, C>(context: C, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
        C: Into<String>,
    {
        PluginError::Fatal {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn classification_transient() {
        let e = PluginError::Transient("network blip".into());
        assert!(e.is_transient());
        assert!(!e.is_permanent());
        assert!(!e.is_fatal());
    }

    #[test]
    fn classification_permanent() {
        let e = PluginError::Permanent("bad input".into());
        assert!(!e.is_transient());
        assert!(e.is_permanent());
        assert!(!e.is_fatal());
    }

    #[test]
    fn classification_unauthorized_is_permanent() {
        let e = PluginError::Unauthorized("need user auth".into());
        assert!(e.is_permanent());
        assert!(!e.is_transient());
        assert!(!e.is_fatal());
    }

    #[test]
    fn classification_timeout_is_permanent() {
        let e = PluginError::Timeout {
            elapsed_ms: 5000,
            deadline_ms: 3000,
        };
        assert!(e.is_permanent());
        assert!(!e.is_fatal());
    }

    #[test]
    fn classification_resource_exhausted_is_permanent() {
        let e = PluginError::ResourceExhausted {
            resource: "file-descriptors".into(),
        };
        assert!(e.is_permanent());
    }

    #[test]
    fn classification_fatal() {
        let io_err = io::Error::other("disk gone");
        let e = PluginError::fatal("writing state", io_err);
        assert!(!e.is_transient());
        assert!(!e.is_permanent());
        assert!(e.is_fatal());
    }

    #[test]
    fn internal_helper_preserves_source_and_context() {
        let io_err = io::Error::other("oops");
        let e = PluginError::internal("loading config", io_err);
        assert!(e.is_permanent());
        let s = format!("{e}");
        assert!(s.contains("loading config"));
        assert!(s.contains("oops"));
    }

    #[test]
    fn timeout_display_includes_both_values() {
        let e = PluginError::Timeout {
            elapsed_ms: 5000,
            deadline_ms: 3000,
        };
        let s = format!("{e}");
        assert!(s.contains("5000"));
        assert!(s.contains("3000"));
    }

    #[test]
    fn resource_exhausted_display_includes_resource() {
        let e = PluginError::ResourceExhausted {
            resource: "memory".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("memory"));
    }
}
