//! Client-API error envelope built on the cross-boundary
//! [`ErrorClass`] taxonomy.
//!
//! The class enum itself lives in `evo-plugin-sdk` so SDK trait
//! returns ([`evo_plugin_sdk::contract::PluginError`],
//! [`evo_plugin_sdk::contract::ReportError`]) and the wire frame
//! ([`evo_plugin_sdk::wire::WireFrame::Error`]) can carry the same
//! discriminant the steward exposes to consumers on the JSON
//! socket. This module wraps that discriminant in [`ApiError`], the
//! consumer-visible envelope on the client API.

pub use evo_plugin_sdk::error_taxonomy::ErrorClass;
use serde::{Deserialize, Serialize};

/// Structured error envelope returned by every error-bearing
/// surface on the client API.
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
/// `SCHEMAS.md` and grows additively (new subclasses are appended;
/// existing names are stable across releases).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiError {
    /// Top-level taxonomy class.
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
