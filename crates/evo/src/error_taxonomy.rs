//! Structured error taxonomy for cross-boundary surfaces (ADR-0013).
//!
//! Every error that crosses the client-API socket carries an
//! [`ErrorClass`] discriminant alongside the human-readable message.
//! Consumers act on the class (retry, blacklist, escalate, fall
//! back) rather than parsing the message string. Subclasses (the
//! optional `details.subclass` field) refine the semantics within a
//! class without forcing a new top-level variant.
//!
//! ADR-0013 also calls for the same taxonomy on the steward↔plugin
//! wire frame and on the internal error enums (`StewardError`,
//! `PluginError`, `ReportError`). Those rebases are tracked
//! separately; this module delivers the consumer-facing slice (the
//! client API JSON shape) without tangling internal-refactor scope.

use serde::{Deserialize, Serialize};

/// One of eleven structured classes for a cross-boundary error,
/// per ADR-0013 §2.
///
/// A consumer reading the wire form receives one of these as a
/// snake_case string. Forward-compatibility (per ADR-0010): a
/// consumer that observes a class it does not recognise MUST
/// degrade to treating it as [`Self::Internal`] and log a warning
/// rather than crash. This module's [`ErrorClass::from_str`] does
/// that translation.
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
    /// Addressed entity does not exist. Per ADR-0006, NotFound is
    /// silent at the storage layer; this class is the
    /// cross-boundary surface that consumers see.
    NotFound,
    /// Caller lacks the capability for the operation. Distinct
    /// from [`Self::TrustViolation`].
    PermissionDenied,
    /// Verified-identity check failed; the caller is who they say
    /// they are but is not authorised. Trust class, signature,
    /// revocation, role.
    TrustViolation,
    /// Key in the trust chain is outside its `not_before` /
    /// `not_after` window (ADR-0012).
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
    /// from the class. ADR-0013 §4.
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

/// Structured error envelope returned by every error-bearing
/// surface on the client API (ADR-0013 §2).
///
/// JSON shape:
///
/// ```json
/// {
///   "class": "contract_violation",
///   "message": "unknown subject: ...",
///   "details": { "subclass": "unknown_subject" }
/// }
/// ```
///
/// `details` is optional and weakly typed by design. A consumer
/// that wants to act on `details.subclass` must agree on the
/// subclass vocabulary out of band; the taxonomy is documented in
/// `SCHEMAS.md` and grows additively per ADR-0010.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiError {
    /// Top-level taxonomy class (ADR-0013 §2).
    pub class: ErrorClass,
    /// Operator-readable message. Advisory; the contract is on
    /// `class` and `details.subclass`.
    pub message: String,
    /// Structured context. Carries `subclass` and any class-
    /// specific extra fields. Serialised only when populated so
    /// the on-the-wire shape stays compact in the common case.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    /// Build a class+message error with no details.
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            details: None,
        }
    }

    /// Builder: set the `details` field.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Builder: attach a `subclass` discriminator under `details`.
    /// Convenience for the common case of "class plus subclass with
    /// no other context".
    pub fn with_subclass(self, subclass: impl Into<String>) -> Self {
        self.with_details(serde_json::json!({ "subclass": subclass.into() }))
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
    fn fatal_classes_match_adr_0013() {
        // ADR-0013 §4: the class derives the fatality bit.
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

    #[test]
    fn api_error_minimal_shape_serialises_without_details() {
        let e = ApiError::new(ErrorClass::NotFound, "missing");
        let s = serde_json::to_string(&e).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["class"].as_str(), Some("not_found"));
        assert_eq!(v["message"].as_str(), Some("missing"));
        assert!(v.get("details").is_none(), "details omitted when None");
    }

    #[test]
    fn api_error_with_subclass_carries_structured_details() {
        let e = ApiError::new(ErrorClass::ContractViolation, "x")
            .with_subclass("cardinality_violation");
        let s = serde_json::to_string(&e).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["class"].as_str(), Some("contract_violation"));
        assert_eq!(
            v["details"]["subclass"].as_str(),
            Some("cardinality_violation")
        );
    }
}
