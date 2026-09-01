//! Structured errors for surfaces that are intentionally NOT
//! in the current scope.
//!
//! Two errors surface here:
//!
//! - [`CarrierNotInScope`] — non-IP presence carriers (BLE,
//!   Wake-on-LAN, out-of-band-via-chain-bytes) are named in
//!   the broader multi-carrier-announce design but not
//!   currently shipped. Any operator surface or wire-op call
//!   site that attempts to invoke one of these carriers MUST
//!   return this structured error rather than silently
//!   no-op'ing. The five currently shipped carriers
//!   (cached-endpoint dial / subnet sweep TCP / mDNS-SD
//!   targeted query / UDP broadcast wake / audio-plane
//!   hello) are the explicit complement.
//!
//! - [`DeviceClassNotInScope`] — MCU-class follower-mode
//!   devices are not admittable under the current contract.
//!   The framework's admission gate refuses devices that do
//!   not satisfy the full-participation contract (Ed25519
//!   trust anchor + audio-plane TCP listener on port 7331 +
//!   heartbeat emitter on UDP/5354 + audio-plane data plane
//!   participation OR explicit `role = "auto"` non-
//!   participation). Admission attempts from devices outside
//!   that contract surface this error.
//!
//! ## Why these errors exist as standalone types
//!
//! Both scope boundaries are negative constraints — they say
//! "we explicitly do NOT do X". Without an operator-visible
//! refusal, a future operator gesture against a not-in-scope
//! surface would silently no-op (because the surface has no
//! implementation), making the constraint invisible. These
//! structured errors are the operator-visible surface that
//! makes the negative constraint legible — they appear in
//! happenings, diagnostics, and operator UI without the
//! operator having to read the implementation source to
//! discover them.
//!
//! ## Stability
//!
//! Both error types ship even though no production code path
//! returns them yet. Their existence in the public API is
//! what reviewers + future operator-surface authors check
//! against when wiring new gestures: if you find yourself
//! wiring a BLE or WoL carrier, you return
//! `CarrierNotInScope` (the variant exists; you don't have
//! to add it). If you find yourself wiring an MCU follower
//! admission path, you return `DeviceClassNotInScope`. Both
//! errors are removed when an amending decision adopts the
//! corresponding surface; that removal IS the adoption
//! signal.

use serde::{Deserialize, Serialize};

/// Operator gesture (or plugin call) attempted to invoke a
/// non-IP presence carrier that is intentionally not
/// currently shipped.
///
/// Carrier names this error reports include `ble`, `wol`
/// (Wake-on-LAN), and `oob_chain_bytes` (out-of-band
/// chain-bytes transport). The five carriers that ARE
/// shipped — `cached_endpoint_dial`, `subnet_sweep_tcp`,
/// `mdns_targeted_query`, `udp_broadcast_wake`,
/// `audio_plane_hello` — never produce this error.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error,
)]
#[error(
    "carrier `{carrier}` is not currently in scope. Five IP-based \
     carriers are shipped: cached_endpoint_dial, subnet_sweep_tcp, \
     mdns_targeted_query, udp_broadcast_wake, audio_plane_hello."
)]
pub struct CarrierNotInScope {
    /// The carrier name the operator attempted to invoke
    /// (typically `ble` / `wol` / `oob_chain_bytes`).
    pub carrier: String,
}

/// Admission attempt was made by a device that does not
/// satisfy the current full-participation contract.
///
/// `declared_class` typically holds the role string the
/// device's manifest declared (`follower` is the canonical
/// not-in-scope value); `required_capabilities` lists the
/// surfaces the current full-participation contract requires
/// the device to implement at admission time.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error,
)]
#[error(
    "device {device_id} declared class `{declared_class}` is not \
     currently in scope. Required capabilities: {required_capabilities:?}"
)]
pub struct DeviceClassNotInScope {
    /// Canonical id of the device whose admission was
    /// refused.
    pub device_id: String,
    /// The class / role the device's manifest declared
    /// (typically `follower`).
    pub declared_class: String,
    /// The capability surfaces the current contract requires
    /// for full-participation admission.
    pub required_capabilities: Vec<String>,
}

impl CarrierNotInScope {
    /// Build a fresh `CarrierNotInScope` for the named
    /// carrier. Helper so call sites consistently produce
    /// the same error shape.
    pub fn for_carrier(carrier: impl Into<String>) -> Self {
        Self {
            carrier: carrier.into(),
        }
    }
}

impl DeviceClassNotInScope {
    /// Build the canonical refusal — names the full-
    /// participation capability surfaces the framework
    /// requires for admission. Call sites use this when they
    /// have classified an admission attempt as
    /// not-in-scope without needing to redeclare the
    /// capability list.
    pub fn full_participation_required(
        device_id: impl Into<String>,
        declared_class: impl Into<String>,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            declared_class: declared_class.into(),
            required_capabilities: vec![
                "ed25519_trust_anchor".to_string(),
                "audio_plane_tcp_listener:7331".to_string(),
                "heartbeat_emitter_udp:5354".to_string(),
                "audio_plane_data_plane_participation_or_role_auto".to_string(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_not_in_scope_names_unshipped_carrier() {
        let e = CarrierNotInScope::for_carrier("ble");
        assert_eq!(e.carrier, "ble");
        let display = e.to_string();
        assert!(display.contains("ble"));
        assert!(display.contains("not currently in scope"));
    }

    #[test]
    fn carrier_not_in_scope_serializes_round_trip() {
        let e = CarrierNotInScope::for_carrier("wol");
        let json = serde_json::to_string(&e).unwrap();
        let round_tripped: CarrierNotInScope =
            serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, e);
    }

    #[test]
    fn device_class_not_in_scope_names_full_participation_capabilities() {
        let e = DeviceClassNotInScope::full_participation_required(
            "dev-abc", "follower",
        );
        assert_eq!(e.device_id, "dev-abc");
        assert_eq!(e.declared_class, "follower");
        // The contract names the four required capabilities;
        // refusing PRs that admit reduced-capability devices
        // requires the operator surface to see all four.
        assert!(e
            .required_capabilities
            .iter()
            .any(|c| c == "ed25519_trust_anchor"));
        assert!(e
            .required_capabilities
            .iter()
            .any(|c| c.starts_with("audio_plane_tcp_listener")));
        assert!(e
            .required_capabilities
            .iter()
            .any(|c| c.starts_with("heartbeat_emitter_udp")));
        assert!(e
            .required_capabilities
            .iter()
            .any(|c| { c.contains("audio_plane_data_plane_participation") }));
    }

    #[test]
    fn device_class_not_in_scope_display_renders_canonical_refusal() {
        let e = DeviceClassNotInScope::full_participation_required(
            "dev-x", "follower",
        );
        let display = e.to_string();
        assert!(display.contains("dev-x"));
        assert!(display.contains("follower"));
        assert!(display.contains("not currently in scope"));
    }

    #[test]
    fn device_class_not_in_scope_serializes_round_trip() {
        let e = DeviceClassNotInScope::full_participation_required(
            "dev-y", "follower",
        );
        let json = serde_json::to_string(&e).unwrap();
        let round_tripped: DeviceClassNotInScope =
            serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, e);
    }
}
