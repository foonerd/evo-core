//! Cross-boundary error taxonomy.
//!
//! Every error that crosses an evo contract surface — the wire
//! protocol, the client-API socket, the SDK trait return — carries
//! an [`ErrorClass`] discriminant alongside its human-readable
//! message. Consumers act on the class (retry, blacklist, escalate,
//! fall back) rather than parsing the message string.
//!
//! The class is the same on every surface so a translation
//! between layers is lossless: an error originating inside the
//! steward and surfaced to a wire plugin via the wire `Error`
//! frame, then onward to a client through the JSON envelope, is
//! the same class at every step. Subclasses (carried in
//! `details.subclass` on the JSON envelope) refine the semantics
//! within a class without forcing a new top-level variant.

use serde::{Deserialize, Serialize};

/// One of eleven structured classes for a cross-boundary error.
///
/// A consumer reading the wire form receives one of these as a
/// snake_case string. Forward-compatibility: a consumer that
/// observes a class it does not recognise MUST degrade to treating
/// it as [`Self::Internal`] and log a warning rather than crash.
/// New classes are added only with a wire-version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorClass {
    /// Operation may succeed on retry without state change. Network
    /// blip, momentary lock contention.
    Transient,
    /// Plugin or backend currently down; retry with backoff.
    Unavailable,
    /// Quota, memory, disk; retry once pressure relieves.
    ResourceExhausted,
    /// Caller violated the contract (wrong shape, wrong type,
    /// cardinality breach, undeclared predicate). Retrying with the
    /// same input is pointless.
    ContractViolation,
    /// Addressed entity does not exist. NotFound is silent at the
    /// storage layer; this class is the cross-boundary surface
    /// that consumers see.
    NotFound,
    /// Caller lacks the capability for the operation. Distinct
    /// from [`Self::TrustViolation`].
    PermissionDenied,
    /// Verified-identity check failed; the caller is who they say
    /// they are but is not authorised. Trust class, signature,
    /// revocation, role.
    TrustViolation,
    /// Key in the trust chain is outside its `not_before` /
    /// `not_after` window.
    TrustExpired,
    /// The wire frame itself is malformed, the version handshake
    /// failed, or the codec disagreed. Caller should treat as
    /// fatal for the connection.
    ProtocolViolation,
    /// Operator-level configuration error; retrying without
    /// operator action is pointless.
    Misconfiguration,
    /// An invariant the steward upholds was violated internally.
    /// The caller did nothing wrong; the steward will log and
    /// continue, the operation does not retry-cleanly.
    Internal,
}

impl ErrorClass {
    /// Whether an error of this class indicates the connection
    /// itself is unusable.
    ///
    /// Derived (not stored): the wire layer no longer carries an
    /// independent `fatal: bool` — every consumer computes this
    /// from the class.
    pub fn is_connection_fatal(self) -> bool {
        matches!(
            self,
            Self::ProtocolViolation
                | Self::TrustViolation
                | Self::TrustExpired
                | Self::Internal
        )
    }

    /// Whether retry of the SAME request is useful.
    ///
    /// The complement is "the operation requires caller or
    /// operator action before retry can help". Rough cut intended
    /// as guidance for retry harnesses; consumers MAY override.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Transient | Self::Unavailable | Self::ResourceExhausted
        )
    }

    /// Snake-case string form (matches the wire serde encoding).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Unavailable => "unavailable",
            Self::ResourceExhausted => "resource_exhausted",
            Self::ContractViolation => "contract_violation",
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::TrustViolation => "trust_violation",
            Self::TrustExpired => "trust_expired",
            Self::ProtocolViolation => "protocol_violation",
            Self::Misconfiguration => "misconfiguration",
            Self::Internal => "internal",
        }
    }
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_serialises_as_snake_case() {
        let s = serde_json::to_string(&ErrorClass::ContractViolation).unwrap();
        assert_eq!(s, "\"contract_violation\"");
        let s = serde_json::to_string(&ErrorClass::TrustExpired).unwrap();
        assert_eq!(s, "\"trust_expired\"");
    }

    #[test]
    fn class_round_trips_through_serde() {
        for c in [
            ErrorClass::Transient,
            ErrorClass::Unavailable,
            ErrorClass::ResourceExhausted,
            ErrorClass::ContractViolation,
            ErrorClass::NotFound,
            ErrorClass::PermissionDenied,
            ErrorClass::TrustViolation,
            ErrorClass::TrustExpired,
            ErrorClass::ProtocolViolation,
            ErrorClass::Misconfiguration,
            ErrorClass::Internal,
        ] {
            let s = serde_json::to_string(&c).unwrap();
            let back: ErrorClass = serde_json::from_str(&s).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn fatal_classes_are_derived_from_taxonomy() {
        // The class derives the fatality bit; the wire layer
        // never carries an independent `fatal: bool` field.
        assert!(ErrorClass::ProtocolViolation.is_connection_fatal());
        assert!(ErrorClass::Internal.is_connection_fatal());
        assert!(ErrorClass::TrustViolation.is_connection_fatal());
        assert!(ErrorClass::TrustExpired.is_connection_fatal());

        assert!(!ErrorClass::Transient.is_connection_fatal());
        assert!(!ErrorClass::ContractViolation.is_connection_fatal());
        assert!(!ErrorClass::NotFound.is_connection_fatal());
    }

    #[test]
    fn retryable_classes_are_the_pressure_classes() {
        assert!(ErrorClass::Transient.is_retryable());
        assert!(ErrorClass::Unavailable.is_retryable());
        assert!(ErrorClass::ResourceExhausted.is_retryable());

        assert!(!ErrorClass::ContractViolation.is_retryable());
        assert!(!ErrorClass::NotFound.is_retryable());
        assert!(!ErrorClass::TrustViolation.is_retryable());
    }
}
